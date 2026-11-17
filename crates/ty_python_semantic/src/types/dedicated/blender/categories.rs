//! The categories of the Blender rules that accept code the typechecker cannot prove right and only
//! report it: an Assumption, an Implied narrowing or an Imprecise annotation.
//!
//! In each category the code is accepted and typed as its precise form, so a rule's level only
//! decides whether and how it is reported. A category's name sets the level of all its rules in
//! `[rules]` and `[[overrides]]` (`blender-imprecise-annotations = "ignore"`), above `all` and below
//! a rule's own name, and the `[blender] strictness` preset sets the levels of whole categories.

/// A category of Blender rules, named in `[rules]` by [`BlenderRuleCategory::name`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlenderRuleCategory {
    Assumptions,
    ImpliedNarrowings,
    ImpreciseAnnotations,
}

impl BlenderRuleCategory {
    /// The categories, in the order of the `[blender] strictness` presets' columns.
    pub const ALL: [Self; 3] = [
        Self::Assumptions,
        Self::ImpliedNarrowings,
        Self::ImpreciseAnnotations,
    ];

    /// The category's name in `[rules]`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Assumptions => "blender-assumptions",
            Self::ImpliedNarrowings => "blender-implied-narrowings",
            Self::ImpreciseAnnotations => "blender-imprecise-annotations",
        }
    }

    /// The names of the category's rules.
    pub const fn rules(self) -> &'static [&'static str] {
        match self {
            Self::Assumptions => &[
                "blender-assumed-context-member",
                "blender-assumed-list-item",
                "blender-assumed-registration",
                "blender-assumed-complete-registration",
                "blender-assumed-preferences",
            ],
            Self::ImpliedNarrowings => &[
                "blender-narrowed-by-poll",
                "blender-narrowed-by-menu-prefix",
                "blender-narrowed-by-popover",
            ],
            Self::ImpreciseAnnotations => &[
                "blender-imprecise-operator-return",
                "blender-imprecise-enum-value",
                "blender-imprecise-callback-parameter",
            ],
        }
    }

    /// The category named `name` in `[rules]`, if it is one.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|category| category.name() == name)
    }
}
