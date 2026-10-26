//! Talks to the script server (tele.johorstudio.com) on a background thread.

use std::sync::mpsc::Sender;
use std::thread;

use serde::Deserialize;

#[allow(dead_code)]
#[derive(Deserialize, Clone, Debug)]
pub struct Script {
    pub code: String,
    pub title: String,
    pub name: String,
    pub content: String,
    pub updated: i64,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ListItem {
    pub code: String,
    pub title: String,
    pub name: String,
    pub email: String,
    pub updated: i64,
    pub chars: i64,
}

pub enum NetMsg {
    Script(Result<Script, String>),
    List(Result<Vec<ListItem>, String>),
    /// (code, result) of an admin delete.
    Deleted(String, Result<(), String>),
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn get_json<T: for<'de> Deserialize<'de>>(url: &str, token: &str) -> Result<T, String> {
    let mut req = agent().get(url);
    if !token.is_empty() {
        req = req.header("Authorization", &format!("Bearer {token}"));
    }
    let mut resp = req.call().map_err(|e| crate::tr!("Network: {e}", "Rangkaian: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| crate::tr!("Read: {e}", "Baca: {e}"))?;
    if status >= 400 {
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| format!("HTTP {status}"));
        return Err(msg);
    }
    serde_json::from_str(&body).map_err(|e| format!("JSON: {e}"))
}

pub fn fetch_script(
    base: String,
    token: String,
    code: String,
    tx: Sender<NetMsg>,
    ctx: eframe::egui::Context,
) {
    thread::spawn(move || {
        let url = format!("{}/api/scripts/{}", base.trim_end_matches('/'), code.trim());
        let _ = tx.send(NetMsg::Script(get_json(&url, &token)));
        ctx.request_repaint();
    });
}

pub fn fetch_list(base: String, token: String, tx: Sender<NetMsg>, ctx: eframe::egui::Context) {
    thread::spawn(move || {
        let url = format!("{}/api/admin/scripts", base.trim_end_matches('/'));
        let r = if token.is_empty() {
            Err(crate::i18n::t("Admin token not set (Server settings)", "Token admin belum diisi (Tetapan)").to_string())
        } else {
            get_json(&url, &token)
        };
        let _ = tx.send(NetMsg::List(r));
        ctx.request_repaint();
    });
}

pub fn delete_script(
    base: String,
    token: String,
    code: String,
    tx: Sender<NetMsg>,
    ctx: eframe::egui::Context,
) {
    thread::spawn(move || {
        let url = format!("{}/api/admin/scripts/{}", base.trim_end_matches('/'), code);
        let r = (|| {
            if token.is_empty() {
                return Err(crate::i18n::t("Admin token not set (Server settings)", "Token admin belum diisi (Tetapan)").to_string());
            }
            let resp = agent()
                .delete(&url)
                .header("Authorization", &format!("Bearer {token}"))
                .call()
                .map_err(|e| crate::tr!("Network: {e}", "Rangkaian: {e}"))?;
            let status = resp.status().as_u16();
            if status >= 400 {
                return Err(format!("HTTP {status}"));
            }
            Ok(())
        })();
        let _ = tx.send(NetMsg::Deleted(code, r));
        ctx.request_repaint();
    });
}
