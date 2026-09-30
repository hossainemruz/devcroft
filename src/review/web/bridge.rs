use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Message {
    pub token: String,
    pub seq: u64,
    pub op: String,
    pub capture: Option<String>,
    pub version: Option<u64>,
    pub data: Value,
}
pub(super) struct Gate {
    token: String,
    last: u64,
}
impl Gate {
    pub fn new() -> Self {
        Self {
            token: super::super::session::digest(rand::random::<[u8; 32]>()),
            last: 0,
        }
    }
    pub fn token(&self) -> &str {
        &self.token
    }
    pub fn accept(&mut self, body: &str) -> Result<Message> {
        ensure!(body.len() <= 64 * 1024, "Oversized review message");
        let m: Message = serde_json::from_str(body)?;
        ensure!(
            m.token == self.token && m.seq > self.last && m.seq <= 9_007_199_254_740_991,
            "Invalid or stale review capability"
        );
        ensure!(
            [
                "ready",
                "source",
                "location",
                "draft",
                "finding",
                "resolve",
                "examined",
                "generate",
                "repair",
                "guide",
                "ask",
                "stop",
                "copy",
                "revision",
                "reload",
                "preview",
                "publish",
                "reconcile"
            ]
            .contains(&m.op.as_str()),
            "Unsupported review operation"
        );
        self.last = m.seq;
        Ok(m)
    }
}
pub(super) fn script_json(value: &impl serde::Serialize) -> Result<String> {
    Ok(serde_json::to_string(value)?
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frames_cannot_forge_tokens_or_replay() {
        let mut g = Gate::new();
        let body = |token: &str, seq| {
            serde_json::json!({"token":token,"seq":seq,"op":"ready","capture":null,"version":null,"data":{}}).to_string()
        };
        assert!(g.accept(&body("forged", 1)).is_err());
        let valid = body(g.token(), 1);
        assert!(g.accept(&valid).is_ok());
        assert!(g.accept(&valid).is_err());
        let mut replacement = Gate::new();
        assert!(replacement.accept(&valid).is_err());
    }
    #[test]
    fn data_never_terminates_shell_script() {
        let s = script_json(&"</script><script>alert(1)</script>\u{2028}").unwrap();
        assert!(!s.contains('<'));
        assert!(!s.contains('\u{2028}'));
    }
}
