//! The categories of the Blender rules that accept code the typechecker cannot prove right and only
//! report it: an Assumption, an Implied narrowing or an Imprecise annotation.
//!
//! In each category the code is accepted and typed as its precise form, so a rule's level only
//! decides whether and how it is reported. A category's name sets the level of all its rules in
//! `[rules]` and `[[overrides]]` (`blender-imprecise-annotations = "ignore"`), above `all` and below
//! a rule's own name, and the `[blender] strictness` preset sets the levels of whole categories.
//! The table lists every rule of a category, also those not declared yet: names the registry does
//! not know are skipped.

/// A category of Blender rules: its name in `[rules]` and the names of its rules.
#[derive(Debug)]
pub struct BlenderRuleCategory {
    pub name: &'static str,
    pub rules: &'static [&'static str],
}

/// The Blender rule categories, in the order of the `[blender] strictness` presets' columns.
pub const BLENDER_RULE_CATEGORIES: [BlenderRuleCategory; 3] = [
    BlenderRuleCategory {
        name: "blender-assumptions",
        rules: &[
            "blender-assumed-context-member",
            "blender-assumed-list-item",
            "blender-assumed-registration",
            "blender-assumed-complete-registration",
            "blender-assumed-preferences",
        ],
    },
    BlenderRuleCategory {
        name: "blender-implied-narrowings",
        rules: &[
            "blender-narrowed-by-poll",
            "blender-narrowed-by-menu-prefix",
            "blender-narrowed-by-popover",
        ],
    },
    BlenderRuleCategory {
        name: "blender-imprecise-annotations",
        rules: &[
            "blender-imprecise-operator-return",
            "blender-imprecise-enum-value",
            "blender-imprecise-callback-parameter",
        ],
    },
];

/// The category named `name` in `[rules]`, if it is one.
pub fn blender_rule_category(name: &str) -> Option<&'static BlenderRuleCategory> {
    BLENDER_RULE_CATEGORIES
        .iter()
        .find(|category| category.name == name)
}
