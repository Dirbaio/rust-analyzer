//! Completion for (built-in) attributes, derives and lints.
//!
//! This module uses a bit of static metadata to provide completions for builtin-in attributes and lints.

use ide_db::{
    generated::lints::{
        Lint, CLIPPY_LINTS, CLIPPY_LINT_GROUPS, DEFAULT_LINTS, FEATURES, RUSTDOC_LINTS,
    },
    syntax_helpers::node_ext::parse_tt_as_comma_sep_paths,
    SymbolKind,
};
use itertools::Itertools;
use once_cell::sync::Lazy;
use rustc_hash::FxHashMap;
use syntax::{
    ast::{self, AttrKind},
    AstNode, SyntaxKind, T,
};

use crate::{
    completions::module_or_attr,
    context::{CompletionContext, PathCompletionCtx, PathKind, PathQualifierCtx},
    item::CompletionItem,
    Completions,
};

mod cfg;
mod derive;
mod lint;
mod repr;

/// Complete inputs to known builtin attributes as well as derive attributes
pub(crate) fn complete_known_attribute_input(
    acc: &mut Completions,
    ctx: &CompletionContext,
) -> Option<()> {
    let attribute = ctx.fake_attribute_under_caret.as_ref()?;
    let name_ref = match attribute.path() {
        Some(p) => Some(p.as_single_name_ref()?),
        None => None,
    };
    let (path, tt) = name_ref.zip(attribute.token_tree())?;
    if tt.l_paren_token().is_none() {
        return None;
    }

    match path.text().as_str() {
        "repr" => repr::complete_repr(acc, ctx, tt),
        "derive" => derive::complete_derive(acc, ctx, ctx.attr.as_ref()?),
        "feature" => lint::complete_lint(acc, ctx, &parse_tt_as_comma_sep_paths(tt)?, FEATURES),
        "allow" | "warn" | "deny" | "forbid" => {
            let existing_lints = parse_tt_as_comma_sep_paths(tt)?;

            let lints: Vec<Lint> = CLIPPY_LINT_GROUPS
                .iter()
                .map(|g| &g.lint)
                .chain(DEFAULT_LINTS)
                .chain(CLIPPY_LINTS)
                .chain(RUSTDOC_LINTS)
                .cloned()
                .collect();

            lint::complete_lint(acc, ctx, &existing_lints, &lints);
        }
        "cfg" => {
            cfg::complete_cfg(acc, ctx);
        }
        _ => (),
    }
    Some(())
}

pub(crate) fn complete_attribute(acc: &mut Completions, ctx: &CompletionContext) {
    let (is_absolute_path, qualifier, is_inner, annotated_item_kind) = match ctx.path_context {
        Some(PathCompletionCtx {
            kind: Some(PathKind::Attr { kind, annotated_item_kind }),
            is_absolute_path,
            ref qualifier,
            ..
        }) => (is_absolute_path, qualifier, kind == AttrKind::Inner, annotated_item_kind),
        _ => return,
    };

    match qualifier {
        Some(PathQualifierCtx { resolution, is_super_chain, .. }) => {
            if *is_super_chain {
                acc.add_keyword(ctx, "super::");
            }

            let module = match resolution {
                Some(hir::PathResolution::Def(hir::ModuleDef::Module(it))) => it,
                _ => return,
            };

            for (name, def) in module.scope(ctx.db, ctx.module) {
                if let Some(def) = module_or_attr(ctx.db, def) {
                    acc.add_resolution(ctx, name, def);
                }
            }
            return;
        }
        // fresh use tree with leading colon2, only show crate roots
        None if is_absolute_path => acc.add_crate_roots(ctx),
        // only show modules in a fresh UseTree
        None => {
            ctx.process_all_names(&mut |name, def| {
                if let Some(def) = module_or_attr(ctx.db, def) {
                    acc.add_resolution(ctx, name, def);
                }
            });
            acc.add_nameref_keywords(ctx);
        }
    }

    let attributes = annotated_item_kind.and_then(|kind| {
        if ast::Expr::can_cast(kind) {
            Some(EXPR_ATTRIBUTES)
        } else {
            KIND_TO_ATTRIBUTES.get(&kind).copied()
        }
    });

    let add_completion = |attr_completion: &AttrCompletion| {
        let mut item =
            CompletionItem::new(SymbolKind::Attribute, ctx.source_range(), attr_completion.label);

        if let Some(lookup) = attr_completion.lookup {
            item.lookup_by(lookup);
        }

        if let Some((snippet, cap)) = attr_completion.snippet.zip(ctx.config.snippet_cap) {
            item.insert_snippet(cap, snippet);
        }

        if is_inner || !attr_completion.prefer_inner {
            item.add_to(acc);
        }
    };

    match attributes {
        Some(applicable) => applicable
            .iter()
            .flat_map(|name| ATTRIBUTES.binary_search_by(|attr| attr.key().cmp(name)).ok())
            .flat_map(|idx| ATTRIBUTES.get(idx))
            .for_each(add_completion),
        None if is_inner => ATTRIBUTES.iter().for_each(add_completion),
        None => ATTRIBUTES.iter().filter(|compl| !compl.prefer_inner).for_each(add_completion),
    }
}

struct AttrCompletion {
    label: &'static str,
    lookup: Option<&'static str>,
    snippet: Option<&'static str>,
    prefer_inner: bool,
}

impl AttrCompletion {
    fn key(&self) -> &'static str {
        self.lookup.unwrap_or(self.label)
    }

    const fn prefer_inner(self) -> AttrCompletion {
        AttrCompletion { prefer_inner: true, ..self }
    }
}

const fn attr(
    label: &'static str,
    lookup: Option<&'static str>,
    snippet: Option<&'static str>,
) -> AttrCompletion {
    AttrCompletion { label, lookup, snippet, prefer_inner: false }
}

macro_rules! attrs {
    // attributes applicable to all items
    [@ { item $($tt:tt)* } {$($acc:tt)*}] => {
        attrs!(@ { $($tt)* } { $($acc)*, "deprecated", "doc", "dochidden", "docalias", "must_use", "no_mangle" })
    };
    // attributes applicable to all adts
    [@ { adt $($tt:tt)* } {$($acc:tt)*}] => {
        attrs!(@ { $($tt)* } { $($acc)*, "derive", "repr" })
    };
    // attributes applicable to all linkable things aka functions/statics
    [@ { linkable $($tt:tt)* } {$($acc:tt)*}] => {
        attrs!(@ { $($tt)* } { $($acc)*, "export_name", "link_name", "link_section" })
    };
    // error fallback for nicer error message
    [@ { $ty:ident $($tt:tt)* } {$($acc:tt)*}] => {
        compile_error!(concat!("unknown attr subtype ", stringify!($ty)))
    };
    // general push down accumulation
    [@ { $lit:literal $($tt:tt)*} {$($acc:tt)*}] => {
        attrs!(@ { $($tt)* } { $($acc)*, $lit })
    };
    [@ {$($tt:tt)+} {$($tt2:tt)*}] => {
        compile_error!(concat!("Unexpected input ", stringify!($($tt)+)))
    };
    // final output construction
    [@ {} {$($tt:tt)*}] => { &[$($tt)*] as _ };
    // starting matcher
    [$($tt:tt),*] => {
        attrs!(@ { $($tt)* } { "allow", "cfg", "cfg_attr", "deny", "forbid", "warn" })
    };
}

#[rustfmt::skip]
static KIND_TO_ATTRIBUTES: Lazy<FxHashMap<SyntaxKind, &[&str]>> = Lazy::new(|| {
    use SyntaxKind::*;
    [
        (
            SOURCE_FILE,
            attrs!(
                item,
                "crate_name", "feature", "no_implicit_prelude", "no_main", "no_std",
                "recursion_limit", "type_length_limit", "windows_subsystem"
            ),
        ),
        (MODULE, attrs!(item, "macro_use", "no_implicit_prelude", "path")),
        (ITEM_LIST, attrs!(item, "no_implicit_prelude")),
        (MACRO_RULES, attrs!(item, "macro_export", "macro_use")),
        (MACRO_DEF, attrs!(item)),
        (EXTERN_CRATE, attrs!(item, "macro_use", "no_link")),
        (USE, attrs!(item)),
        (TYPE_ALIAS, attrs!(item)),
        (STRUCT, attrs!(item, adt, "non_exhaustive")),
        (ENUM, attrs!(item, adt, "non_exhaustive")),
        (UNION, attrs!(item, adt)),
        (CONST, attrs!(item)),
        (
            FN,
            attrs!(
                item, linkable,
                "cold", "ignore", "inline", "must_use", "panic_handler", "proc_macro",
                "proc_macro_derive", "proc_macro_attribute", "should_panic", "target_feature",
                "test", "track_caller"
            ),
        ),
        (STATIC, attrs!(item, linkable, "global_allocator", "used")),
        (TRAIT, attrs!(item, "must_use")),
        (IMPL, attrs!(item, "automatically_derived")),
        (ASSOC_ITEM_LIST, attrs!(item)),
        (EXTERN_BLOCK, attrs!(item, "link")),
        (EXTERN_ITEM_LIST, attrs!(item, "link")),
        (MACRO_CALL, attrs!()),
        (SELF_PARAM, attrs!()),
        (PARAM, attrs!()),
        (RECORD_FIELD, attrs!()),
        (VARIANT, attrs!("non_exhaustive")),
        (TYPE_PARAM, attrs!()),
        (CONST_PARAM, attrs!()),
        (LIFETIME_PARAM, attrs!()),
        (LET_STMT, attrs!()),
        (EXPR_STMT, attrs!()),
        (LITERAL, attrs!()),
        (RECORD_EXPR_FIELD_LIST, attrs!()),
        (RECORD_EXPR_FIELD, attrs!()),
        (MATCH_ARM_LIST, attrs!()),
        (MATCH_ARM, attrs!()),
        (IDENT_PAT, attrs!()),
        (RECORD_PAT_FIELD, attrs!()),
    ]
    .into_iter()
    .collect()
});
const EXPR_ATTRIBUTES: &[&str] = attrs!();

/// <https://doc.rust-lang.org/reference/attributes.html#built-in-attributes-index>
// Keep these sorted for the binary search!
const ATTRIBUTES: &[AttrCompletion] = &[
    attr("allow(…)", Some("allow"), Some("allow(${0:lint})")),
    attr("automatically_derived", None, None),
    attr("cfg(…)", Some("cfg"), Some("cfg(${0:predicate})")),
    attr("cfg_attr(…)", Some("cfg_attr"), Some("cfg_attr(${1:predicate}, ${0:attr})")),
    attr("cold", None, None),
    attr(r#"crate_name = """#, Some("crate_name"), Some(r#"crate_name = "${0:crate_name}""#))
        .prefer_inner(),
    attr("deny(…)", Some("deny"), Some("deny(${0:lint})")),
    attr(r#"deprecated"#, Some("deprecated"), Some(r#"deprecated"#)),
    attr("derive(…)", Some("derive"), Some(r#"derive(${0:Debug})"#)),
    attr(r#"doc = "…""#, Some("doc"), Some(r#"doc = "${0:docs}""#)),
    attr(r#"doc(alias = "…")"#, Some("docalias"), Some(r#"doc(alias = "${0:docs}")"#)),
    attr(r#"doc(hidden)"#, Some("dochidden"), Some(r#"doc(hidden)"#)),
    attr(
        r#"export_name = "…""#,
        Some("export_name"),
        Some(r#"export_name = "${0:exported_symbol_name}""#),
    ),
    attr("feature(…)", Some("feature"), Some("feature(${0:flag})")).prefer_inner(),
    attr("forbid(…)", Some("forbid"), Some("forbid(${0:lint})")),
    attr("global_allocator", None, None),
    attr(r#"ignore = "…""#, Some("ignore"), Some(r#"ignore = "${0:reason}""#)),
    attr("inline", Some("inline"), Some("inline")),
    attr("link", None, None),
    attr(r#"link_name = "…""#, Some("link_name"), Some(r#"link_name = "${0:symbol_name}""#)),
    attr(
        r#"link_section = "…""#,
        Some("link_section"),
        Some(r#"link_section = "${0:section_name}""#),
    ),
    attr("macro_export", None, None),
    attr("macro_use", None, None),
    attr(r#"must_use"#, Some("must_use"), Some(r#"must_use"#)),
    attr("no_implicit_prelude", None, None).prefer_inner(),
    attr("no_link", None, None).prefer_inner(),
    attr("no_main", None, None).prefer_inner(),
    attr("no_mangle", None, None),
    attr("no_std", None, None).prefer_inner(),
    attr("non_exhaustive", None, None),
    attr("panic_handler", None, None),
    attr(r#"path = "…""#, Some("path"), Some(r#"path ="${0:path}""#)),
    attr("proc_macro", None, None),
    attr("proc_macro_attribute", None, None),
    attr("proc_macro_derive(…)", Some("proc_macro_derive"), Some("proc_macro_derive(${0:Trait})")),
    attr(
        r#"recursion_limit = "…""#,
        Some("recursion_limit"),
        Some(r#"recursion_limit = "${0:128}""#),
    )
    .prefer_inner(),
    attr("repr(…)", Some("repr"), Some("repr(${0:C})")),
    attr("should_panic", Some("should_panic"), Some(r#"should_panic"#)),
    attr(
        r#"target_feature = "…""#,
        Some("target_feature"),
        Some(r#"target_feature = "${0:feature}""#),
    ),
    attr("test", None, None),
    attr("track_caller", None, None),
    attr("type_length_limit = …", Some("type_length_limit"), Some("type_length_limit = ${0:128}"))
        .prefer_inner(),
    attr("used", None, None),
    attr("warn(…)", Some("warn"), Some("warn(${0:lint})")),
    attr(
        r#"windows_subsystem = "…""#,
        Some("windows_subsystem"),
        Some(r#"windows_subsystem = "${0:subsystem}""#),
    )
    .prefer_inner(),
];

fn parse_comma_sep_expr(input: ast::TokenTree) -> Option<Vec<ast::Expr>> {
    let r_paren = input.r_paren_token()?;
    let tokens = input
        .syntax()
        .children_with_tokens()
        .skip(1)
        .take_while(|it| it.as_token() != Some(&r_paren));
    let input_expressions = tokens.into_iter().group_by(|tok| tok.kind() == T![,]);
    Some(
        input_expressions
            .into_iter()
            .filter_map(|(is_sep, group)| (!is_sep).then(|| group))
            .filter_map(|mut tokens| syntax::hacks::parse_expr_from_str(&tokens.join("")))
            .collect::<Vec<ast::Expr>>(),
    )
}

#[test]
fn attributes_are_sorted() {
    let mut attrs = ATTRIBUTES.iter().map(|attr| attr.key());
    let mut prev = attrs.next().unwrap();

    attrs.for_each(|next| {
        assert!(
            prev < next,
            r#"ATTRIBUTES array is not sorted, "{}" should come after "{}""#,
            prev,
            next
        );
        prev = next;
    });
}
