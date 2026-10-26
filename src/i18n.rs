//! Two UI languages: English (default) and Bahasa Melayu.
//!
//! `t("English", "Melayu")` picks a fixed string; `tr!("en {x}", "ms {x}")`
//! (or with positional args) formats one.

use std::sync::atomic::{AtomicBool, Ordering};

static MS: AtomicBool = AtomicBool::new(false);

pub fn set_lang(code: &str) {
    MS.store(code == "ms", Ordering::Relaxed);
}

pub fn ms() -> bool {
    MS.load(Ordering::Relaxed)
}

pub fn t(en: &'static str, ms_text: &'static str) -> &'static str {
    if ms() { ms_text } else { en }
}

#[macro_export]
macro_rules! tr {
    ($en:literal, $ms:literal) => {
        if $crate::i18n::ms() { format!($ms) } else { format!($en) }
    };
    ($en:literal, $ms:literal, $($arg:tt)*) => {
        if $crate::i18n::ms() { format!($ms, $($arg)*) } else { format!($en, $($arg)*) }
    };
}
