//! The `[blender]` options: how strict the Blender rules that accept code and only report it are.
//!
//! Those rules come in three categories ([`BlenderRuleCategory`]): Assumptions, Implied narrowings
//! and Imprecise annotations. A category's name sets the level of all its rules in `[rules]`, and
//! the `strictness` preset sets the levels of all three categories at once, below every explicit
//! level, `all` included:
//!
//! | Preset | Assumptions | Implied narrowings | Imprecise annotations |
//! |---|---|---|---|
//! | `lenient` | warn | ignore | ignore |
//! | `default` | warn | warn | warn |
//! | `strict` | error | error | error |

use ruff_macros::{Combine, OptionsMetadata};
use ruff_ranged_value::RangedValue;
use serde::{Deserialize, Serialize};
use ty_combine::Combine;
use ty_python_semantic::lint::{Level, LintId, LintRegistry};
use ty_python_semantic::types::BlenderRuleCategory;

use crate::metadata::options::{Options, Rules};

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
    fn level(self, category: BlenderRuleCategory) -> Level {
        match (self, category) {
            (Strictness::Strict, _) => Level::Error,
            (Strictness::Lenient, BlenderRuleCategory::Assumptions) | (Strictness::Default, _) => {
                Level::Warn
            }
            (Strictness::Lenient, _) => Level::Ignore,
        }
    }
}

/// Writes the `[blender] strictness` preset of `options` into its `[rules]`, as a level for every
/// rule of the Blender rule categories placed ahead of all other selectors, `all` included, so
/// that every explicit level takes precedence over it. Called once the selectors are in their
/// order of precedence, as the `[rules]` of `[[overrides]]` and of other configuration files are
/// merged after these.
///
/// Without a preset, the rules' default levels are the `default` preset's.
pub(crate) fn add_strictness_preset(options: &mut Options, registry: &LintRegistry) {
    let Some(strictness) = options
        .blender
        .as_ref()
        .and_then(|blender| blender.strictness.clone())
    else {
        return;
    };
    let preset: Rules = BlenderRuleCategory::ALL
        .into_iter()
        .flat_map(|category| {
            let level = strictness.clone().map_value(|preset| preset.level(category));
            category
                .rules()
                .iter()
                .filter(|name| registry.get(name).is_ok())
                .map(move |name| (level.clone().map_value(|_| (*name).to_string()), level.clone()))
        })
        .collect();
    options.rules = Some(match options.rules.take() {
        Some(rules) => rules.combine(preset),
        None => preset,
    });
}

/// The precedence of a `[rules]` selector among `all` (0), a Blender rule category (1) and the
/// rule names (2). A stable sort by it after upstream's keeps aliases ahead of the rule names.
pub(crate) fn selector_rank(name: &str) -> u8 {
    if name == "all" {
        0
    } else if BlenderRuleCategory::from_name(name).is_some() {
        1
    } else {
        2
    }
}

/// The `[rules]` schema entries of the Blender rule categories, each taking a `level_schema`.
#[cfg(feature = "schemars")]
pub(crate) fn category_schemas(
    level_schema: &schemars::Schema,
) -> impl Iterator<Item = (String, serde_json::Value)> + '_ {
    BlenderRuleCategory::ALL.into_iter().map(move |category| {
        let schema = schemars::json_schema!({
            "title": format!(
                "set the severity level of the Blender rules: {}",
                category.rules().join(", ")
            ),
            "description": "Configure the severity level of every rule of this Blender rule \
                category. Individual rule settings override it; it overrides `all`.",
            "oneOf": [level_schema],
        });
        (category.name().to_string(), schema.into())
    })
}

/// The rules of the Blender rule category `name` names, none when it names no category.
pub(crate) fn category_lints<'a>(
    name: &str,
    registry: &'a LintRegistry,
) -> impl Iterator<Item = LintId> + 'a {
    BlenderRuleCategory::from_name(name)
        .into_iter()
        .flat_map(|category| category.rules().iter())
        .filter_map(|name| registry.get(name).ok())
}

impl Combine for Strictness {
    #[inline(always)]
    fn combine_with(&mut self, _other: Self) {}

    #[inline]
    fn combine(self, _other: Self) -> Self {
        self
    }
}
