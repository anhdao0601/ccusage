pub(super) fn canonical_model_name(model: &str) -> String {
    let model = strip_source_label(model.trim());
    let model = model.rsplit('/').next().unwrap_or(model);
    let mut canonical = model.to_ascii_lowercase();
    if canonical.starts_with("claude-") {
        canonical = canonical.replace(['.', '@'], "-");
    }
    if canonical.starts_with("claude-") && has_date_suffix(&canonical) {
        canonical.truncate(canonical.len() - 9);
    }
    canonical
}

pub(super) fn model_matches_selectors(model: &str, selectors: &[String]) -> bool {
    let canonical = canonical_model_name(model);
    let unqualified = canonical.strip_prefix("claude-").unwrap_or(&canonical);
    selectors.iter().any(|selector| {
        let selector = canonical_model_name(selector);
        canonical == selector
            || unqualified == selector
            || (!selector.bytes().any(|byte| byte.is_ascii_digit())
                && (canonical.starts_with(&format!("{selector}-"))
                    || unqualified.starts_with(&format!("{selector}-"))))
    })
}

pub(super) fn model_group_name(model: &str, selectors: Option<&[String]>) -> String {
    let canonical = canonical_model_name(model);
    let Some(selectors) = selectors else {
        return canonical;
    };
    selectors
        .iter()
        .find(|selector| {
            !selector.bytes().any(|byte| byte.is_ascii_digit())
                && model_matches_selectors(&canonical, std::slice::from_ref(selector))
        })
        .map(|selector| {
            let selector = canonical_model_name(selector);
            selector
                .strip_prefix("claude-")
                .unwrap_or(&selector)
                .to_string()
        })
        .unwrap_or(canonical)
}

fn strip_source_label(model: &str) -> &str {
    let Some(rest) = model.strip_prefix('[') else {
        return model;
    };
    let Some((_, model)) = rest.split_once("] ") else {
        return model;
    };
    model
}

fn has_date_suffix(model: &str) -> bool {
    model.rsplit_once('-').is_some_and(|(_, suffix)| {
        suffix.len() == 8 && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_anthropic_model_aliases_across_sources() {
        for model in [
            "claude-opus-4-6",
            "anthropic/claude-opus-4-6",
            "[pi] anthropic/claude-opus-4.6",
            "claude-opus-4-6-20251001",
        ] {
            assert_eq!(canonical_model_name(model), "claude-opus-4-6");
        }
    }

    #[test]
    fn preserves_official_non_anthropic_version_separators() {
        assert_eq!(canonical_model_name("[pi] openai/gpt-5.4"), "gpt-5.4");
    }

    #[test]
    fn family_selector_matches_versions_and_source_aliases() {
        let selectors = vec!["opus".to_string()];

        assert!(model_matches_selectors("claude-opus-4-6", &selectors));
        assert!(model_matches_selectors(
            "[pi] anthropic/claude-opus-4.7",
            &selectors
        ));
        assert!(!model_matches_selectors("claude-sonnet-4-6", &selectors));
    }
}
