use crate::{BrowserError, IO_TIMEOUT, ManagedBrowser};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Deserialize)]
pub struct DomRegion {
    pub html: String,
    pub scope: String,
    pub bounds: [f64; 4],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Viewport {
    pub data: String,
    pub width: f64,
    pub height: f64,
}

impl ManagedBrowser {
    /// Capture a bounded structural parent region, never document HTML or field values.
    /// # Errors
    /// Fails closed when no unique local ancestor can be located.
    pub async fn repair_region(
        &self,
        selector: &str,
        origin: &Url,
    ) -> Result<DomRegion, BrowserError> {
        self.check_origin(origin).await?;
        let selector = serde_json::to_string(selector).map_err(|_| BrowserError::InvalidAction)?;
        let expression = format!(
            r"(() => {{
            let s={selector}, e=null;
            while(s) {{
                try {{ const n=document.querySelectorAll(s); if(n.length===1) {{e=n[0];break;}} }} catch(_) {{}}
                const cut=Math.max(s.lastIndexOf(' '),s.lastIndexOf('>'));
                if(cut<0) break; s=s.slice(0,cut).trim();
            }}
            if(!e || ['BODY','HTML','SCRIPT','STYLE','TEXTAREA','IFRAME','OBJECT'].includes(e.tagName)) return null;
            const p=e.parentElement;
            if(p && !['BODY','HTML'].includes(p.tagName) && p.querySelectorAll('*').length<=60) e=p;
            if(e.querySelectorAll('*').length>60) return null;
            const r=e.getBoundingClientRect();
            const clone=e.cloneNode(true);
            for(const n of [clone,...clone.querySelectorAll('*')]) {{
                if(['SCRIPT','STYLE','TEXTAREA','IFRAME','OBJECT'].includes(n.tagName)) {{n.remove();continue;}}
                for(const a of [...n.attributes]) if(!['id','class','type','role'].includes(a.name)) n.removeAttribute(a.name);
                for(const c of [...n.childNodes]) if(c.nodeType!==1) c.remove();
            }}
            const html=clone.outerHTML;
            if(html.length>12000) return null;
            let n=e, parts=[];
            while(n && n.nodeType===1) {{
                const siblings=n.parentElement ? [...n.parentElement.children].filter(c=>c.localName===n.localName) : [n];
                parts.unshift(CSS.escape(n.localName)+':nth-of-type('+(siblings.indexOf(n)+1)+')'); n=n.parentElement;
            }}
            return {{html,scope:parts.join(' > '),bounds:[r.x,r.y,r.width,r.height]}};
        }})()"
        );
        tokio::time::timeout(IO_TIMEOUT, self.page.evaluate(expression))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?
            .into_value::<DomRegion>()
            .map_err(|_| BrowserError::InvalidAction)
    }

    /// Verify every candidate match stays within the captured structural region.
    /// # Errors
    /// Rejects escaped targets and targets incompatible with the recorded action.
    pub async fn validate_repair_target(
        &self,
        action: &crate::Action,
        selector: &str,
        scope: &str,
        origin: &Url,
    ) -> Result<(), BrowserError> {
        self.check_origin(origin).await?;
        let selector = serde_json::to_string(selector).map_err(|_| BrowserError::InvalidAction)?;
        let scope = serde_json::to_string(scope).map_err(|_| BrowserError::InvalidAction)?;
        let kind = match action {
            crate::Action::Fill { .. } => "fill",
            crate::Action::Submit { .. } => "submit",
            crate::Action::Click { .. } | crate::Action::DownloadLinks { .. } => "link",
            crate::Action::Navigate { .. } => "wait",
        };
        let expression = format!(
            r"(() => {{
            const root=document.querySelector({scope});
            const nodes=[...document.querySelectorAll({selector})];
            return !!root && nodes.length>0 && nodes.length<=25 && nodes.every(e=> {{
                if(!root.contains(e)) return false;
                if('{kind}'==='fill') return e.tagName==='INPUT' && ['search','date','month','number'].includes(e.type);
                if('{kind}'==='submit') return e.tagName==='FORM' && new URL(e.action,location.href).origin===location.origin && !e.target;
                if('{kind}'==='link') return e.tagName==='A' && !!e.getAttribute('href') && new URL(e.href,location.href).origin===location.origin;
                return true;
            }});
        }})()"
        );
        let valid = tokio::time::timeout(IO_TIMEOUT, self.page.evaluate(expression))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::InvalidAction)?
            .into_value::<bool>()
            .map_err(|_| BrowserError::InvalidAction)?;
        if valid {
            Ok(())
        } else {
            Err(BrowserError::InvalidAction)
        }
    }

    /// Whether the live document reports `readyState === "complete"`.
    ///
    /// Fail-open: any CDP failure reads as complete, so the caller proceeds
    /// to attempt its read instead of stalling on a dead page — the read
    /// itself surfaces the real error with its label.
    pub async fn document_complete(&self) -> bool {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.page.evaluate("document.readyState === 'complete'"),
        )
        .await
        .ok()
        .and_then(std::result::Result::ok)
        .and_then(|value| value.into_value::<bool>().ok())
        .unwrap_or(true)
    }

    /// Whether the page has been network- and DOM-quiet since the previous
    /// probe: no new resource timings and no DOM mutations. The first call
    /// installs a one-shot `MutationObserver` and baseline, and reports not
    /// quiet so the caller allows an observation window.
    ///
    /// SPAs keep fetching after `readyState === "complete"`; polling this
    /// before a screenshot avoids freezing a loading spinner. Fail-open:
    /// any CDP failure reads as quiet.
    pub async fn page_quiet(&self) -> bool {
        const PROBE: &str = r"(() => {
            const w = window;
            if (!w.__clinchSettleProbe) {
                w.__clinchSettleProbe = {
                    res: performance.getEntriesByType('resource').length,
                    mut: 0,
                };
                new MutationObserver(() => { w.__clinchSettleProbe.mut++; })
                    .observe(document, {
                        childList: true, subtree: true,
                        attributes: true, characterData: true,
                    });
                return false;
            }
            const s = w.__clinchSettleProbe;
            const resNow = performance.getEntriesByType('resource').length;
            const newRes = resNow - s.res;
            const newMut = s.mut;
            s.res = resNow;
            s.mut = 0;
            return newRes === 0 && newMut === 0;
        })()";
        tokio::time::timeout(std::time::Duration::from_secs(5), self.page.evaluate(PROBE))
            .await
            .ok()
            .and_then(std::result::Result::ok)
            .and_then(|value| value.into_value::<bool>().ok())
            .unwrap_or(true)
    }

    /// Local-only viewport preview in CSS pixels.
    /// # Errors
    /// Returns CDP or capture errors.
    pub async fn viewport(&self) -> Result<Viewport, BrowserError> {
        use chromiumoxide::cdp::browser_protocol::page::{
            CaptureScreenshotFormat, CaptureScreenshotParams,
        };
        tokio::time::timeout(IO_TIMEOUT, async {
            let dimensions = self
                .page
                .evaluate("[window.innerWidth,window.innerHeight]")
                .await
                // A navigation committing mid-capture destroys the execution
                // context; report the retryable race, not a dead connection.
                .map_err(crate::actions::evaluation_error)?
                .into_value::<[f64; 2]>()
                .map_err(|_| BrowserError::Connection)?;
            let params = CaptureScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Jpeg)
                .quality(80)
                .build();
            let result = self
                .page
                .execute(params)
                .await
                .map_err(|_| BrowserError::Connection)?;
            Ok(Viewport {
                data: String::from(result.result.data),
                width: dimensions[0],
                height: dimensions[1],
            })
        })
        .await
        .map_err(|_| BrowserError::Timeout)?
    }
}
