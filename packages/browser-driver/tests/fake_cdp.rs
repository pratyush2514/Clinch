//! Integration tests for `browser_driver::test_utils::fake_cdp`.
//!
//! Moved out of `src/` so the main source stays test-free.

use browser_driver::test_utils::fake_cdp::*;
use browser_driver::{interactive_elements, select_active_page_target};
use chromiumoxide::cdp::browser_protocol::{
    accessibility::AxNode,
    target::{TargetId, TargetInfo},
};
use std::time::Duration;

fn target_entry(id: &str, kind: &str, url: &str, attached: bool) -> serde_json::Value {
    serde_json::json!({
        "targetId": id,
        "type": kind,
        "title": "fixture",
        "url": url,
        "attached": attached,
        "canAccessOpener": false,
    })
}

#[tokio::test]
async fn test_fake_cdp_cross_origin_process_swap_recovery() -> Result<(), Box<dyn std::error::Error>>
{
    let billing_url = "https://github.com/account/billing/history";
    let fake = FakeCdpServer::start(vec![
        ScriptStep::reply("Accessibility.enable", serde_json::json!({})),
        // Stale session right after the swap: the domain answers, but
        // the tree is empty.
        ScriptStep::reply(
            "Accessibility.getFullAXTree",
            serde_json::json!({"nodes": []}),
        ),
        ScriptStep::reply(
            "Target.getTargets",
            serde_json::json!({"targetInfos": [
                target_entry("github-target", "page", billing_url, true),
            ]}),
        ),
        ScriptStep::reply("Accessibility.enable", serde_json::json!({})),
        // Re-armed session serves the rendered tree.
        ScriptStep::reply("Accessibility.getFullAXTree", billing_history_tree()),
    ])
    .await
    .map_err(|error| format!("fake server failed to start: {error}"))?;
    let run = async {
        let mut client = FakeCdpClient::connect(fake.url()).await?;
        // The exact resync conversation `ax_snapshot` performs: enable,
        // tree, targets, enable, tree.
        client
            .call("Accessibility.enable", serde_json::json!({}))
            .await?;
        let stale = client
            .call("Accessibility.getFullAXTree", serde_json::json!({}))
            .await?;
        let stale_nodes: Vec<AxNode> =
            serde_json::from_value(stale.get("nodes").cloned().unwrap_or_default())
                .map_err(|error| format!("bad stale tree: {error}"))?;
        assert!(
            interactive_elements(&stale_nodes).is_empty(),
            "stale session yields zero candidates, forcing the retry"
        );
        let listing = client
            .call("Target.getTargets", serde_json::json!({}))
            .await?;
        let targets: Vec<TargetInfo> =
            serde_json::from_value(listing.get("targetInfos").cloned().unwrap_or_default())
                .map_err(|error| format!("bad target listing: {error}"))?;
        assert_eq!(targets.len(), 1);
        client
            .call("Accessibility.enable", serde_json::json!({}))
            .await?;
        let rendered = client
            .call("Accessibility.getFullAXTree", serde_json::json!({}))
            .await?;
        let nodes: Vec<AxNode> =
            serde_json::from_value(rendered.get("nodes").cloned().unwrap_or_default())
                .map_err(|error| format!("bad rendered tree: {error}"))?;
        let elements = interactive_elements(&nodes);
        assert_eq!(
            elements
                .iter()
                .map(|element| element.backend_node_id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
            "retry recovers every invoice row, header excluded"
        );
        assert!(
            elements.iter().all(|element| element.name == "Download"),
            "recovered rows carry their visible labels"
        );
        // The conversation ran in production order, with nothing extra
        // and nothing out of sequence.
        assert_eq!(
            fake.received_methods(),
            vec![
                "Accessibility.enable",
                "Accessibility.getFullAXTree",
                "Target.getTargets",
                "Accessibility.enable",
                "Accessibility.getFullAXTree",
            ]
        );
        assert!(fake.violations().is_empty());
        client.close().await;
        Ok::<_, Box<dyn std::error::Error>>(())
    };
    let outcome = tokio::time::timeout(Duration::from_secs(10), run).await;
    fake.shutdown();
    outcome.map_err(|_| "fake CDP roundtrip timed out")??;
    Ok(())
}

#[tokio::test]
async fn test_fake_cdp_portal_reanchoring_drift_guard() -> Result<(), Box<dyn std::error::Error>> {
    let billing_url = "https://github.com/account/billing/history";
    let fake = FakeCdpServer::start(vec![ScriptStep::reply(
        "Target.getTargets",
        serde_json::json!({"targetInfos": [
            target_entry("google-target", "page", "https://google.com/", true),
            target_entry("github-target", "page", billing_url, true),
            target_entry("worker", "service_worker", billing_url, true),
            target_entry("detached", "page", billing_url, false),
        ]}),
    )])
    .await
    .map_err(|error| format!("fake server failed to start: {error}"))?;
    let run = async {
        let mut client = FakeCdpClient::connect(fake.url()).await?;
        let listing = client
            .call("Target.getTargets", serde_json::json!({}))
            .await?;
        // Wire-parsed listing through the real production selector.
        let targets: Vec<TargetInfo> =
            serde_json::from_value(listing.get("targetInfos").cloned().unwrap_or_default())
                .map_err(|error| format!("bad target listing: {error}"))?;
        let url = |raw: &str| url::Url::parse(raw).map_err(|error| format!("bad url: {error}"));
        // Pre-navigation the google tab is active; post-navigation the
        // selection switches to the github tab — the re-anchor.
        let before = select_active_page_target(&targets, &url("https://google.com/")?);
        let after = select_active_page_target(&targets, &url(billing_url)?);
        assert_eq!(before, Some(TargetId::new("google-target")));
        assert_eq!(after, Some(TargetId::new("github-target")));
        assert_ne!(before, after, "origin transition switches target IDs");
        // Unknown destinations and non-page/detached entries never win:
        // the guard keeps the current handle instead of guessing.
        assert_eq!(
            select_active_page_target(&targets, &url("https://other.example/")?),
            None
        );
        assert_eq!(fake.received_methods(), vec!["Target.getTargets"]);
        assert!(fake.violations().is_empty());
        client.close().await;
        Ok::<_, Box<dyn std::error::Error>>(())
    };
    let outcome = tokio::time::timeout(Duration::from_secs(10), run).await;
    fake.shutdown();
    outcome.map_err(|_| "fake CDP roundtrip timed out")??;
    Ok(())
}
