//! Integration tests for `browser_driver::session`.
//!
//! Moved out of `src/` so the main source stays test-free.

use browser_driver::session::*;
use url::Url;

fn url(value: &str) -> Result<Url, url::ParseError> {
    Url::parse(value)
}

#[test]
fn classifies_authenticated_login_and_origin_signals() -> Result<(), Box<dyn std::error::Error>> {
    let portal = url("https://portal.example.com/overview")?;
    assert_eq!(
        detect_auth_signal(&url("https://portal.example.com/files")?, &portal),
        AuthSignal::Authenticated
    );
    assert_eq!(
        detect_auth_signal(&url("https://portal.example.com/login?next=/")?, &portal),
        AuthSignal::LoginRedirect {
            url: "https://portal.example.com/login?next=/".into()
        }
    );
    assert_eq!(
        detect_auth_signal(&url("https://portal.example.com/2fa/challenge")?, &portal),
        AuthSignal::LoginRedirect {
            url: "https://portal.example.com/2fa/challenge".into()
        }
    );
    assert!(
        detect_auth_signal(&url("https://sso.example.net/login")?, &portal)
            .requires_embedded_auth()
    );
    assert!(
        !detect_auth_signal(&url("https://portal.example.com/files")?, &portal)
            .requires_embedded_auth()
    );
    Ok(())
}

#[test]
fn hyphenated_paths_are_not_login_markers() -> Result<(), Box<dyn std::error::Error>> {
    let portal = url("https://portal.example.com/")?;
    // Hyphenated slugs must not trip the challenge detector.
    for path in [
        "/files",
        "/files/history",
        "/account",
        "/verify-file-status",
    ] {
        assert_eq!(
            detect_auth_signal(&url(&format!("https://portal.example.com{path}"))?, &portal),
            AuthSignal::Authenticated,
            "{path} must stay authenticated"
        );
    }
    Ok(())
}

#[test]
fn challenge_markers_match_bot_interstitials() -> Result<(), Box<dyn std::error::Error>> {
    // Cloudflare "Just a moment" title.
    assert!(is_challenge_page(
        "Just a moment...",
        &url("https://claude.ai/")?,
        "Verifying you are human. This may take a few seconds.",
    ));
    // Turnstile body copy without a telling title.
    assert!(is_challenge_page(
        "claude.ai",
        &url("https://claude.ai/")?,
        "Please verify you are human to continue.",
    ));
    // reCAPTCHA wording.
    assert!(is_challenge_page(
        "Login",
        &url("https://example.com/login")?,
        "Please prove you're not a robot: I'm not a robot",
    ));
    // Cloudflare challenge-platform path alone is decisive.
    assert!(is_challenge_page(
        "example",
        &url("https://example.com/cdn-cgi/challenge-platform/h/b")?,
        "ordinary copy",
    ));
    // Cloudflare's "Performing security verification" interstitial
    // copy (observed live on claude.ai): the classic markers miss it.
    assert!(is_challenge_page(
        "claude.ai",
        &url("https://claude.ai/")?,
        "claude.ai\nPerforming security verification\nThis website uses a security service to protect against malicious bots. This page is displayed while the website verifies you are not a bot.",
    ));
    Ok(())
}

#[test]
fn challenge_markers_ignore_plain_pages_and_logins() -> Result<(), Box<dyn std::error::Error>> {
    // A real destination page: no markers anywhere.
    assert!(!is_challenge_page(
        "Claude",
        &url("https://claude.ai/login")?,
        "Your thinking partner for big ambitions",
    ));
    // A login form is not a bot challenge.
    assert!(!is_challenge_page(
        "Sign in",
        &url("https://example.com/signin")?,
        "Enter your email to sign in",
    ));
    Ok(())
}

#[test]
fn challenge_kind_routes_interactive_gates_past_l1() -> Result<(), Box<dyn std::error::Error>> {
    // Reddit's reCAPTCHA parent page: the checkbox lives in a
    // cross-origin iframe, so the parent text carries the gate signal.
    assert_eq!(
        challenge_kind(
            "Reddit - Dive into anything",
            &url("https://www.reddit.com/")?,
            "Prove your humanity\nComplete the challenge below to continue to Reddit.",
        ),
        Some(ChallengeKind::InteractiveGate)
    );
    // Cloudflare interstitial copy stays on the L1 path.
    assert_eq!(
        challenge_kind(
            "Just a moment...",
            &url("https://claude.ai/")?,
            "Verifying you are human. This may take a few seconds.",
        ),
        Some(ChallengeKind::Interstitial)
    );
    // A login form is neither.
    assert_eq!(
        challenge_kind(
            "Sign in",
            &url("https://example.com/signin")?,
            "Enter your email to sign in",
        ),
        None
    );
    Ok(())
}

#[test]
fn auth_state_reads_guest_landings_and_sessions() -> Result<(), Box<dyn std::error::Error>> {
    // Reddit's logged-out feed: sign-up/login CTAs everywhere.
    assert_eq!(
        auth_state_of(
            "Reddit - Dive into anything",
            &url("https://www.reddit.com/")?,
            "Join the most real place on the internet\nContinue with Google\nContinue with Apple\nSign Up\nLog In",
        ),
        AuthState::LoggedOut
    );
    // X's sign-in landing: corroborated markers.
    assert_eq!(
        auth_state_of(
            "X. It's what's happening",
            &url("https://x.com/")?,
            "Happening now.\nContinue with Google\nContinue with Apple\nCreate account\nSign in",
        ),
        AuthState::LoggedOut
    );
    // A login-form URL is decisive on its own.
    assert_eq!(
        auth_state_of(
            "Sign in",
            &url("https://example.com/signin")?,
            "Enter your email",
        ),
        AuthState::LoggedOut
    );
    // A signed-in page advertising "log out" wins over guest markers.
    assert_eq!(
        auth_state_of(
            "Reddit - Dive into anything",
            &url("https://www.reddit.com/")?,
            "u/someone\nHome\nPopular\nLog Out\nSign up for premium",
        ),
        AuthState::Authenticated
    );
    // One weak marker (newsletter CTA) is not a guest landing.
    assert_eq!(
        auth_state_of(
            "Some blog",
            &url("https://blog.example.com/post")?,
            "Great article. Sign up for our newsletter below.",
        ),
        AuthState::Unknown
    );
    // A plain destination page: nothing to classify.
    assert_eq!(
        auth_state_of(
            "Claude",
            &url("https://claude.ai/")?,
            "Welcome back\nHow can I help you today?",
        ),
        AuthState::Unknown
    );
    Ok(())
}

#[test]
fn auth_state_never_masks_a_challenge() -> Result<(), Box<dyn std::error::Error>> {
    // A gated page can carry login copy ("sign in to continue") — the
    // classifier may read it either way, but the service checks the
    // challenge detector first, so the challenge card always wins.
    // This test pins the classifier's honest reading, not the ordering.
    let body = "Prove your humanity\nComplete the challenge below\nSign in to continue";
    assert_eq!(
        challenge_kind("Reddit", &url("https://www.reddit.com/")?, body),
        Some(ChallengeKind::InteractiveGate)
    );
    assert_eq!(
        auth_state_of("Reddit", &url("https://www.reddit.com/")?, body),
        AuthState::Unknown
    );
    Ok(())
}

#[test]
fn identity_provider_hops_are_sso_not_mismatch() -> Result<(), Box<dyn std::error::Error>> {
    let portal = url("https://chatgpt.com/")?;
    // Curated cross-root IdP: expected mid-SSO, still unconnected.
    let signal = detect_auth_signal(&url("https://auth.openai.com/authorize")?, &portal);
    assert_eq!(
        signal,
        AuthSignal::SsoChallenge {
            url: "https://auth.openai.com/authorize".into()
        }
    );
    assert!(signal.requires_embedded_auth());
    // Strict subdomain of the portal: same treatment.
    let portal = url("https://claude.ai/")?;
    assert!(matches!(
        detect_auth_signal(&url("https://auth.claude.ai/login")?, &portal),
        AuthSignal::SsoChallenge { .. }
    ));
    // Unknown foreign origins stay mismatches, never challenges.
    assert!(matches!(
        detect_auth_signal(&url("https://sso.evil.com/login")?, &portal),
        AuthSignal::OriginMismatch { .. }
    ));
    // Same-parent siblings are NOT trusted without a public-suffix list.
    let portal = url("https://app.example.com/")?;
    assert!(matches!(
        detect_auth_signal(&url("https://auth.example.com/login")?, &portal),
        AuthSignal::OriginMismatch { .. }
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH and launches a real browser using a temporary profile"]
async fn storage_hydration_registers_document_script() -> Result<(), Box<dyn std::error::Error>> {
    let executable = std::env::var("CLINCH_CHROMIUM_PATH")?;
    let profile = tempfile::tempdir()?;
    let browser =
        browser_driver::ManagedBrowser::launch(std::path::Path::new(&executable), profile.path())
            .await?;
    // Exercises the `Page.addScriptToEvaluateOnNewDocument` path with a
    // validated additive-only script; the empty call proves the no-op
    // short-circuits before any CDP traffic.
    browser
        .hydrate_local_storage(&[session_sync::StorageItem {
            key: "fixture-session".into(),
            value: "fixture-value".into(),
        }])
        .await?;
    browser.hydrate_local_storage(&[]).await?;
    browser.shutdown().await?;
    profile.close()?;
    Ok(())
}
