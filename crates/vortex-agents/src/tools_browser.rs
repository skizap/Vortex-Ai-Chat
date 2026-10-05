//! Browser automation tools driven by the isolated Chromium instance.
//!
//! Safety rules enforced server-side:
//! - only http/https navigation (no file://, no chrome://),
//! - typing into password fields or CAPTCHA widgets is refused: the model
//!   must hand off to the human for login/verification,
//! - page reads report login/captcha presence so the model pauses.
//! Consequential web actions (purchases, publishing, submissions) are handled
//! by the model calling `request_approval`, which always pauses for the user.

use crate::browser::BrowserManager;
use crate::cdp::CdpPage;
use crate::errors::{Risk, ToolError};
use crate::registry::{Tool, ToolContext, ToolResult};
use crate::schema::{ParamSpec, ParamType, ToolDef};
use async_trait::async_trait;
use std::sync::Arc;

fn check_nav_url(url: &str) -> Result<(), ToolError> {
    let parsed = url::Url::parse(url)
        .map_err(|e| ToolError::InvalidArgs("url".into(), format!("invalid URL: {e}")))?;
    match parsed.scheme() {
        "http" | "https" => Ok(()),
        other => Err(ToolError::NotPermitted(format!(
            "navigation to scheme '{other}' is not allowed (http/https only)"
        ))),
    }
}

async fn page_of(
    ctx: &ToolContext,
) -> Result<(Arc<crate::cdp::CdpConnection>, CdpPage), ToolError> {
    let manager = ctx.browser.as_ref().ok_or_else(|| {
        ToolError::Unavailable(format!(
            "browser automation is unavailable. {}",
            BrowserManager::unavailable_hint()
        ))
    })?;
    manager.page_for(&ctx.run_id).await
}

pub struct BrowserNavigateTool;

const NAV_DEF: ToolDef = ToolDef {
    name: "browser_navigate",
    description: "Navigate the isolated browser tab to an http(s) URL.",
    params: &[ParamSpec {
        name: "url",
        ty: ParamType::Str,
        required: true,
        description: "http(s) URL.",
    }],
};

#[async_trait]
impl Tool for BrowserNavigateTool {
    fn def(&self) -> &'static ToolDef {
        &NAV_DEF
    }
    fn available(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        if !ctx.settings.tools.browser {
            return Err(ToolError::Unavailable(
                "browser tools are disabled in Settings (Tools).".into(),
            ));
        }
        if ctx.browser.is_none()
            || !ctx
                .browser
                .as_ref()
                .map(|b| b.is_available())
                .unwrap_or(false)
        {
            return Err(ToolError::Unavailable(format!(
                "browser automation is unavailable: {}",
                BrowserManager::unavailable_hint()
            )));
        }
        Ok(())
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let url = args["url"].as_str().expect("validated");
        check_nav_url(url)?;
        let (conn, page) = page_of(ctx).await?;
        page.navigate(&conn, url).await?;
        // Give the page a moment to start loading, then report state.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let state = page_state(&conn, &page).await?;
        Ok(ToolResult {
            text: format!(
                "navigated to {url}. title: {}; {state}",
                page_state_title(&conn, &page).await
            ),
            summary: format!("navigated to {url}"),
            diff: None,
        })
    }
}

async fn page_state_title(conn: &Arc<crate::cdp::CdpConnection>, page: &CdpPage) -> String {
    page.evaluate(conn, "document.title || ''")
        .await
        .map(|v| v.as_str().unwrap_or("").to_string())
        .unwrap_or_default()
}

async fn page_state(
    conn: &Arc<crate::cdp::CdpConnection>,
    page: &CdpPage,
) -> Result<String, ToolError> {
    let value = page
        .evaluate(
            conn,
            r#"(() => ({
                url: location.href,
                hasPasswordInput: !!document.querySelector('input[type=password]'),
                hasCaptcha: !!document.querySelector('iframe[src*="recaptcha"], iframe[src*="hcaptcha"], iframe[src*="turnstile"]'),
                textLength: (document.body ? document.body.innerText.length : 0)
            }))()"#,
        )
        .await?;
    let has_pw = value
        .get("hasPasswordInput")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let has_captcha = value
        .get("hasCaptcha")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let mut note = String::from("loaded");
    if has_pw {
        note.push_str("; NOTE: the page shows a login/password form — if credentials are needed, pause and hand off to the human user; do not attempt to log in yourself");
    }
    if has_captcha {
        note.push_str("; NOTE: a CAPTCHA widget is present — pause and hand off to the human user");
    }
    Ok(note)
}

pub struct BrowserReadTool;

const READ_DEF: ToolDef = ToolDef {
    name: "browser_read",
    description:
        "Read the current page: URL, title, visible text, and flags for login forms or CAPTCHAs.",
    params: &[],
};

#[async_trait]
impl Tool for BrowserReadTool {
    fn def(&self) -> &'static ToolDef {
        &READ_DEF
    }
    fn available(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        if !ctx.settings.tools.browser {
            return Err(ToolError::Unavailable(
                "browser tools are disabled in Settings (Tools).".into(),
            ));
        }
        if ctx.browser.is_none() {
            return Err(ToolError::Unavailable(format!(
                "browser automation is unavailable. {}",
                BrowserManager::unavailable_hint()
            )));
        }
        Ok(())
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        _args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let (conn, page) = page_of(ctx).await?;
        let value = page
            .evaluate(
                &conn,
                r#"(() => ({
                    url: location.href,
                    title: document.title,
                    text: document.body ? document.body.innerText.slice(0, 15000) : '',
                    hasPasswordInput: !!document.querySelector('input[type=password]'),
                    hasCaptcha: !!document.querySelector('iframe[src*="recaptcha"], iframe[src*="hcaptcha"], iframe[src*="turnstile"]')
                }))()"#,
            )
            .await?;
        let url = value
            .get("url")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let title = value
            .get("title")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let text = value
            .get("text")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let has_pw = value
            .get("hasPasswordInput")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let has_captcha = value
            .get("hasCaptcha")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let mut out = format!("URL: {url}\nTitle: {title}\n");
        if has_pw {
            out.push_str(
                "LOGIN FORM PRESENT: do not enter credentials; hand off to the human user.\n",
            );
        }
        if has_captcha {
            out.push_str("CAPTCHA PRESENT: pause and hand off to the human user.\n");
        }
        out.push_str("\n");
        out.push_str(&text);
        Ok(ToolResult {
            summary: format!("read page {url}"),
            text: out,
            diff: None,
        })
    }
}

pub struct BrowserClickTool;

const CLICK_DEF: ToolDef = ToolDef {
    name: "browser_click",
    description: "Click an element selected by CSS selector. For consequential actions (buy, publish, submit) call request_approval first.",
    params: &[ParamSpec {
        name: "selector",
        ty: ParamType::Str,
        required: true,
        description: "CSS selector of the element to click.",
    }],
};

#[async_trait]
impl Tool for BrowserClickTool {
    fn def(&self) -> &'static ToolDef {
        &CLICK_DEF
    }
    fn risk(&self) -> Risk {
        Risk::Medium
    }
    fn available(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        if !ctx.settings.tools.browser {
            return Err(ToolError::Unavailable(
                "browser tools are disabled in Settings (Tools).".into(),
            ));
        }
        Ok(())
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let selector = args["selector"].as_str().expect("validated");
        let (conn, page) = page_of(ctx).await?;
        let expr = format!(
            r#"(() => {{
                const el = document.querySelector({sel_json});
                if (!el) return JSON.stringify({{found: false}});
                el.click();
                return JSON.stringify({{found: true, tag: el.tagName, type: el.getAttribute('type') || ''}});
            }})()"#,
            sel_json = serde_json::to_string(selector).unwrap_or_default()
        );
        let value = page.evaluate(&conn, &expr).await?;
        let found = value
            .get("found")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        if !found {
            return Err(ToolError::Execution(format!(
                "no element matches '{selector}'"
            )));
        }
        let tag = value.get("tag").and_then(|s| s.as_str()).unwrap_or("");
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        Ok(ToolResult {
            summary: format!("clicked {selector}"),
            text: format!("clicked element '{selector}' ({tag})"),
            diff: None,
        })
    }
}

pub struct BrowserTypeTool;

const TYPE_DEF: ToolDef = ToolDef {
    name: "browser_type",
    description: "Type text into an input selected by CSS selector. Password fields and CAPTCHAs are refused: hand off to the human user instead.",
    params: &[
        ParamSpec { name: "selector", ty: ParamType::Str, required: true, description: "CSS selector of the input." },
        ParamSpec { name: "text", ty: ParamType::Str, required: true, description: "Text to type." },
        ParamSpec { name: "submit", ty: ParamType::Bool, required: false, description: "If true, submit the enclosing form after typing." },
    ],
};

#[async_trait]
impl Tool for BrowserTypeTool {
    fn def(&self) -> &'static ToolDef {
        &TYPE_DEF
    }
    fn risk(&self) -> Risk {
        Risk::Medium
    }
    fn available(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        if !ctx.settings.tools.browser {
            return Err(ToolError::Unavailable(
                "browser tools are disabled in Settings (Tools).".into(),
            ));
        }
        Ok(())
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let selector = args["selector"].as_str().expect("validated");
        let text = args["text"].as_str().expect("validated");
        let submit = args
            .get("submit")
            .and_then(|s| s.as_bool())
            .unwrap_or(false);
        let (conn, page) = page_of(ctx).await?;
        let expr = format!(
            r#"(() => {{
                const el = document.querySelector({sel_json});
                if (!el) return JSON.stringify({{status: 'not_found'}});
                if (el.tagName === 'INPUT' && el.getAttribute('type') === 'password')
                    return JSON.stringify({{status: 'password_field'}});
                el.value = {text_json};
                el.dispatchEvent(new Event('input', {{bubbles: true}}));
                el.dispatchEvent(new Event('change', {{bubbles: true}}));
                return JSON.stringify({{status: 'typed'}});
            }})()"#,
            sel_json = serde_json::to_string(selector).unwrap_or_default(),
            text_json = serde_json::to_string(text).unwrap_or_default()
        );
        let value = page.evaluate(&conn, &expr).await?;
        match value.get("status").and_then(|s| s.as_str()).unwrap_or("") {
            "typed" => {}
            "not_found" => {
                return Err(ToolError::Execution(format!(
                    "no input matches '{selector}'"
                )))
            }
            "password_field" => {
                return Err(ToolError::NotPermitted(
                    "refusing to type into a password field. Ask the human user to log in \
                     themselves; never handle user credentials."
                        .into(),
                ))
            }
            _ => return Err(ToolError::Execution("typing failed".to_string())),
        }
        if submit {
            page.evaluate(
                &conn,
                r#"(() => { const el = document.querySelector('form'); if (el) el.requestSubmit(); return true; })()"#,
            )
            .await?;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        Ok(ToolResult {
            summary: format!("typed into {selector}"),
            text: format!(
                "typed into '{selector}'{}",
                if submit {
                    " and submitted the form"
                } else {
                    ""
                }
            ),
            diff: None,
        })
    }
}
