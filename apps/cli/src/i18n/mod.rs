use fluent_bundle::{FluentArgs, FluentBundle, FluentResource};
use unic_langid::LanguageIdentifier;

const ZH_CN: &str = include_str!("locales/zh-CN.ftl");
const EN_US: &str = include_str!("locales/en-US.ftl");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Locale {
    ZhCn,
    #[default]
    EnUs,
}

impl Locale {
    pub fn parse(input: &str) -> Result<Self, String> {
        match input.trim().to_ascii_lowercase().as_str() {
            "zh" | "zh-cn" | "zh_cn" => Ok(Self::ZhCn),
            "en" | "en-us" | "en_us" => Ok(Self::EnUs),
            other => Err(format!("unsupported locale `{other}`; use zh-CN or en-US")),
        }
    }

    pub fn detect() -> Self {
        // Future config precedence lives here:
        // 1. persisted config locale from core
        // 2. explicit CLI --locale
        // 3. environment locale below
        for key in ["ZLOGIC_LANG", "LC_ALL", "LC_MESSAGES", "LANG"] {
            let Ok(value) = std::env::var(key) else {
                continue;
            };
            if let Ok(locale) = Self::parse(&value) {
                return locale;
            }
            let lower = value.to_ascii_lowercase();
            if lower.starts_with("zh") {
                return Self::ZhCn;
            }
            if lower.starts_with("en") || lower == "c" || lower == "posix" {
                return Self::EnUs;
            }
        }
        Self::default()
    }

    pub fn resolve(input: &str) -> Result<Self, String> {
        if input.trim().eq_ignore_ascii_case("auto") {
            Ok(Self::detect())
        } else {
            Self::parse(input)
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::EnUs => "en-US",
        }
    }

    fn langid(self) -> LanguageIdentifier {
        match self {
            Self::ZhCn => "zh-CN",
            Self::EnUs => "en-US",
        }
        .parse()
        .expect("bundled locale tag must be valid")
    }

    fn source(self) -> &'static str {
        match self {
            Self::ZhCn => ZH_CN,
            Self::EnUs => EN_US,
        }
    }
}

pub struct I18n {
    locale: Locale,
    bundle: FluentBundle<FluentResource>,
}

impl I18n {
    pub fn new(locale: Locale) -> Self {
        let resource = FluentResource::try_new(locale.source().to_string())
            .expect("bundled Fluent resource must parse");
        let mut bundle = FluentBundle::new(vec![locale.langid()]);
        bundle.set_use_isolating(false);
        bundle
            .add_resource(resource)
            .expect("bundled Fluent resource ids must be unique");
        Self { locale, bundle }
    }

    pub fn locale(&self) -> Locale {
        self.locale
    }

    pub fn set_locale(&mut self, locale: Locale) {
        *self = Self::new(locale);
    }

    pub fn t(&self, id: &str) -> String {
        self.format(id, None)
    }

    pub fn wire(&self, message: &zlogic_protocol::LocalizedMessage) -> String {
        let id = message.key.replace('.', "-").replace('_', "-");
        let Some(msg) = self.bundle.get_message(&id) else {
            return message.fallback.clone();
        };
        let Some(pattern) = msg.value() else {
            return message.fallback.clone();
        };
        let mut args = FluentArgs::new();
        for (name, value) in &message.args {
            let key = name.as_str();
            match value {
                serde_json::Value::Number(number) => {
                    if let Some(int) = number.as_i64() {
                        args.set(key, int);
                    } else if let Some(float) = number.as_f64() {
                        args.set(key, float);
                    }
                }
                serde_json::Value::String(text) => {
                    args.set(key, text.as_str());
                }
                serde_json::Value::Bool(flag) => {
                    args.set(key, if *flag { "true" } else { "false" });
                }
                _ => {}
            }
        }
        let mut errors = Vec::new();
        self.bundle
            .format_pattern(pattern, Some(&args), &mut errors)
            .to_string()
    }

    pub fn count(&self, id: &str, count: usize) -> String {
        let mut args = FluentArgs::new();
        args.set("count", count as i64);
        self.format(id, Some(&args))
    }

    pub fn format(&self, id: &str, args: Option<&FluentArgs<'_>>) -> String {
        let Some(message) = self.bundle.get_message(id) else {
            return id.to_string();
        };
        let Some(pattern) = message.value() else {
            return id.to_string();
        };
        let mut errors = Vec::new();
        self.bundle
            .format_pattern(pattern, args, &mut errors)
            .to_string()
    }
}

impl Default for I18n {
    fn default() -> Self {
        Self::new(Locale::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Extract top-level Fluent message ids from source text. Per the Fluent
    /// grammar, message lines start at column 0 with `identifier =`; comments
    /// start with `#`, terms with `-`, and continuation/attribute lines are
    /// indented — so this is exact for well-formed resources (and
    /// `ids_resolve_in_their_own_bundle` below guards the extraction itself).
    fn message_ids(src: &str) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        for line in src.lines() {
            let Some(first) = line.chars().next() else {
                continue;
            };
            if first.is_whitespace() || first == '#' || first == '-' {
                continue;
            }
            let Some((id, _)) = line.split_once('=') else {
                continue;
            };
            let id = id.trim();
            if !id.is_empty()
                && id.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                ids.insert(id.to_string());
            }
        }
        ids
    }

    #[test]
    fn locales_have_identical_message_id_sets() {
        let en = message_ids(EN_US);
        let zh = message_ids(ZH_CN);
        let missing_in_zh: Vec<_> = en.difference(&zh).collect();
        let missing_in_en: Vec<_> = zh.difference(&en).collect();
        assert!(
            missing_in_zh.is_empty() && missing_in_en.is_empty(),
            "locale files out of sync:\n  in en-US but missing from zh-CN: {missing_in_zh:?}\n  in zh-CN but missing from en-US: {missing_in_en:?}"
        );
        assert!(
            !en.is_empty(),
            "id extraction found nothing — grammar drift?"
        );
    }

    #[test]
    fn ids_resolve_in_their_own_bundle() {
        // Guards the textual extraction in `message_ids` AND proves both
        // resources parse cleanly through the same path `I18n::new` uses.
        for locale in [Locale::EnUs, Locale::ZhCn] {
            let i18n = I18n::new(locale);
            for id in message_ids(locale.source()) {
                assert!(
                    i18n.bundle.get_message(&id).is_some(),
                    "{}: extracted id `{id}` not found by fluent-bundle",
                    locale.tag()
                );
            }
        }
    }

    #[test]
    fn resolve_accepts_auto_and_both_tags() {
        assert!(Locale::resolve("auto").is_ok());
        assert!(Locale::resolve("AUTO").is_ok());
        assert_eq!(Locale::resolve("zh-CN"), Ok(Locale::ZhCn));
        assert_eq!(Locale::resolve("ZH_cn"), Ok(Locale::ZhCn));
        assert_eq!(Locale::resolve("zh"), Ok(Locale::ZhCn));
        assert_eq!(Locale::resolve("en-US"), Ok(Locale::EnUs));
        assert_eq!(Locale::resolve("EN"), Ok(Locale::EnUs));
        assert_eq!(Locale::resolve(" en-us "), Ok(Locale::EnUs));
    }

    #[test]
    fn resolve_rejects_junk() {
        assert!(Locale::resolve("fr-FR").is_err());
        assert!(Locale::resolve("klingon").is_err());
        assert!(Locale::resolve("").is_err());
        assert!(Locale::resolve("zh-TW").is_err());
    }

    #[test]
    fn parameterized_key_formats_in_both_locales() {
        // `sessions-count` takes `$count` (with an English plural rule).
        let en = I18n::new(Locale::EnUs);
        assert_eq!(en.count("sessions-count", 1), "1 session");
        assert_eq!(en.count("sessions-count", 3), "3 sessions");

        let zh = I18n::new(Locale::ZhCn);
        assert_eq!(zh.count("sessions-count", 3), "3 个会话");
    }

    #[test]
    fn missing_key_falls_back_to_the_id() {
        let i18n = I18n::new(Locale::EnUs);
        assert_eq!(
            i18n.t("definitely-not-a-real-key"),
            "definitely-not-a-real-key"
        );
    }

    #[test]
    fn wire_resolves_catalogue_keys_and_falls_back() {
        let zh = I18n::new(Locale::ZhCn);
        let en = I18n::new(Locale::EnUs);

        let with_key = |key: &str| zlogic_protocol::LocalizedMessage {
            key: key.into(),
            args: [("error".to_string(), serde_json::json!("no key"))]
                .into_iter()
                .collect(),
            fallback: "FALLBACK".to_string(),
        };
        assert_eq!(
            zh.wire(&with_key("error.submit_no_usable_model")),
            "这条消息没有发送：没有可用的模型（no key）。请配置模型并设置 API key 后重新发送。"
        );
        assert!(
            en.wire(&with_key("notice.mcp_tools_pending"))
                .contains("Still fetching the tool manifest"),
            "{}",
            en.wire(&with_key("notice.mcp_tools_pending"))
        );
        assert_eq!(en.wire(&with_key("error.some_future_code")), "FALLBACK");
    }
}
