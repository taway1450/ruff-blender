//! The `[blender]` options: how strict the Blender rules that accept code and only report it are.
//!
//! Those rules come in three categories (`ty_python_semantic::types::BLENDER_RULE_CATEGORIES`):
//! Assumptions, Implied narrowings and Imprecise annotations. A category's name sets the level of
//! all its rules in `[rules]`, and the `strictness` preset sets the levels of all three categories
//! at once, below every explicit level:
//!
//! | Preset | Assumptions | Implied narrowings | Imprecise annotations |
//! |---|---|---|---|
//! | `lenient` | warn | ignore | ignore |
//! | `default` | warn | warn | warn |
//! | `strict` | error | error | error |

use ruff_db::diagnostic::Severity;
use ruff_macros::{Combine, OptionsMetadata};
use ruff_ranged_value::RangedValue;
use serde::{Deserialize, Serialize};
use ty_combine::Combine;
use ty_python_semantic::lint::{Level, LintRegistry, LintSource, RuleSelection};
use ty_python_semantic::types::{BLENDER_RULE_CATEGORIES, BlenderRuleCategory};

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Hash,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct BlenderOptions {
    /// How much the Blender rules may assume: the level of each category of the Blender rules that
    /// accept code the typechecker cannot prove right and only report it.
    ///
    /// - `"lenient"`: Assumptions warn; Implied narrowings and Imprecise annotations are ignored.
    /// - `"default"`: all three categories warn.
    /// - `"strict"`: all three categories are errors.
    ///
    /// The code is accepted under every preset. A level set in `[rules]` for a category
    /// (`blender-imprecise-annotations = "ignore"`), a rule or `all` overrides the preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#""default""#,
        value_type = r#""lenient" | "default" | "strict""#,
        example = r#"
            strictness = "strict"
        "#
    )]
    pub strictness: Option<RangedValue<Strictness>>,
}

impl BlenderOptions {
    pub(crate) fn strictness(options: Option<&Self>) -> Option<Strictness> {
        options
            .and_then(|options| options.strictness.as_deref())
            .copied()
    }
}

/// A `[blender] strictness` preset.
#[derive(
    Debug, Default, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize, get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum Strictness {
    /// Assumptions warn; Implied narrowings and Imprecise annotations are ignored.
    Lenient,
    /// Assumptions, Implied narrowings and Imprecise annotations warn.
    #[default]
    Default,
    /// Assumptions, Implied narrowings and Imprecise annotations are errors.
    Strict,
}

impl Strictness {
    /// The level of `category` under this preset.
    fn level(self, category: &BlenderRuleCategory) -> Level {
        match (self, category.name) {
            (Strictness::Strict, _) => Level::Error,
            (Strictness::Lenient, "blender-assumptions") | (Strictness::Default, _) => Level::Warn,
            (Strictness::Lenient, _) => Level::Ignore,
        }
    }

    /// Sets the level of every rule of the Blender rule categories in `selection` to this preset's,
    /// before the explicit levels of `[rules]` are applied over it.
    pub(crate) fn apply(self, selection: &mut RuleSelection, registry: &LintRegistry) {
        for category in &BLENDER_RULE_CATEGORIES {
            let level = self.level(category);
            for lint in category
                .rules
                .iter()
                .filter_map(|name| registry.get(name).ok())
            {
                match Severity::try_from(level) {
                    Ok(severity) => selection.enable(lint, severity, LintSource::Default),
                    Err(()) => selection.disable(lint),
                }
            }
        }
    }
}

impl Combine for Strictness {
    #[inline(always)]
    fn combine_with(&mut self, _other: Self) {}

    #[inline]
    fn combine(self, _other: Self) -> Self {
        self
    }
}
