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
                .map_err(|_| BrowserError::Connection)?
                .into_value::<[f64; 2]>()
                .map_err(|_| BrowserError::Connection)?;
            let params = CaptureScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Jpeg)
                .quality(65)
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
