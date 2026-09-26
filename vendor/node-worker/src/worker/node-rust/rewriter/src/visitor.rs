use oxc::{
    allocator::{Allocator, HashMap, Vec},
    ast::ast,
    ast_visit::{Visit, walk},
    semantic::Scoping,
    span::{GetSpan, Span},
    syntax::{operator::UpdateOperator, symbol::SymbolId},
};

use crate::changes::{JsChanges, change};

// {ident}.register([..deps calculated from Vec<SystemJsModule> -> name..], function({ident}_export, {ident}_context) {
//     return {
//         setters: [...calculated from Vec<SystemJsModule> -> imports],
//     };
// }, [..meta skipped..])
//

#[derive(Clone, Copy)]
pub enum NamedMapImported<'data> {
    /// an identifier name (rendered bare, or quoted when used as an export-name string)
    Ident(&'data str),
    /// the RAW source string literal, including its quotes and escapes
    Literal(&'data str),
}
impl<'data> NamedMapImported<'data> {
    pub fn new(name: &ast::ModuleExportName<'data>) -> NamedMapImported<'data> {
        use ast::ModuleExportName as M;
        match name {
            M::IdentifierName(name) => NamedMapImported::Ident(name.name.as_str()),
            M::IdentifierReference(nref) => NamedMapImported::Ident(nref.name.as_str()),
            M::StringLiteral(lit) => NamedMapImported::Literal(lit.raw.unwrap().as_str()),
        }
    }

    /// the inner slice: the identifier for `Ident`, the raw (quoted) literal for `Literal`
    pub fn name(&self) -> &'data str {
        match self {
            NamedMapImported::Ident(s) | NamedMapImported::Literal(s) => s,
        }
    }

    pub fn is_ident(&self) -> bool {
        matches!(self, NamedMapImported::Ident(_))
    }
}

pub struct NamedMap<'data> {
    pub local: NamedMapImported<'data>,
    pub external: NamedMapImported<'data>,
}
impl<'data> NamedMap<'data> {
    pub fn new(external: &ast::ModuleExportName<'data>, local: &ast::ModuleExportName<'data>) -> Self {
        Self { external: NamedMapImported::new(external), local: NamedMapImported::new(local) }
    }

    pub fn new_ident(imported: &ast::ModuleExportName<'data>, local: &'data str) -> Self {
        Self { external: NamedMapImported::new(imported), local: NamedMapImported::Ident(local) }
    }
}

pub struct SystemJsDep<'alloc, 'data> {
    pub idx: usize,
    pub raw: &'data str,

    pub default_imports: Vec<'alloc, &'data str>,
    pub named_imports: Vec<'alloc, NamedMap<'data>>,
    pub star_imports: Vec<'alloc, &'data str>,

    pub reexports: Vec<'alloc, NamedMap<'data>>,
    pub star_ns_reexports: Vec<'alloc, NamedMapImported<'data>>,
    pub star_reexport: bool,
}
impl<'alloc, 'data> SystemJsDep<'alloc, 'data> {
    pub fn new(alloc: &'alloc Allocator, idx: usize, raw: &'data str) -> Self {
        Self {
            idx,
            raw,
            default_imports: Vec::new_in(&alloc),
            named_imports: Vec::new_in(&alloc),
            star_imports: Vec::new_in(&alloc),
            reexports: Vec::new_in(&alloc),
            star_ns_reexports: Vec::new_in(&alloc),
            star_reexport: false,
        }
    }
}

/// A top-level function declaration relocated (via `LayoutPiece::Move`) out of `execute` into the
/// register callback body, so it is hoisted before the module executes — matching ESM, where every
/// top-level declaration shares one module scope and function declarations are available before
/// evaluation. `exported` is `Some(name)` for `export function`/`export default function` (also
/// emitted as an `_export` at instantiation), `None` for a plain top-level function.
pub struct HoistedFn<'data> {
    pub span: Span,
    pub exported: Option<NamedMapImported<'data>>,
    pub local: &'data str,
}

pub struct SystemJsModule<'alloc, 'data> {
    pub has_tla: bool,
    pub deps: HashMap<'alloc, &'data str, SystemJsDep<'alloc, 'data>>,
    /// import locals + `export const/let/var` locals — declared as `var …;` in the callback body
    pub hoisted_idents: Vec<'alloc, &'data str>,
    pub hoisted_fns: Vec<'alloc, HoistedFn<'data>>,
}

/// How a top-level import binding is fed by its dep's setter — used to route `import x; export { x }`
/// re-exports into the setter (so they stay live) rather than exporting a stale value in `execute`.
#[derive(Clone, Copy)]
enum ImportBinding<'data> {
    /// `import { imported as local } from source`
    Named { source: &'data str, imported: NamedMapImported<'data> },
    /// `import local from source`
    Default { source: &'data str },
    /// `import * as local from source`
    Star { source: &'data str },
}

pub struct Visitor<'alloc, 'data, 'sema> {
    alloc: &'alloc Allocator,
    scoping: &'sema Scoping,

    /// name of the async-context holder global (JS-owned; passed in), referenced by the await wraps
    ctx: &'data str,
    /// current await-nesting depth, used to order sibling await closers (see `emit_await_wrap`)
    await_depth: u32,

    pub jschanges: JsChanges<'alloc, 'data>,
    pub module: SystemJsModule<'alloc, 'data>,
    pub fn_depth: usize,

    /// module-scope binding (by symbol) -> the name(s) it is exported under, for live-binding tracking
    exported_symbols: HashMap<'alloc, SymbolId, Vec<'alloc, NamedMapImported<'data>>>,
    /// import binding (by symbol) -> its source dep, for routing re-exports of imports into setters
    import_bindings: HashMap<'alloc, SymbolId, ImportBinding<'data>>,
}

impl<'alloc, 'data, 'sema> Visitor<'alloc, 'data, 'sema> {
    pub fn new(
        alloc: &'alloc Allocator,
        ident: &'data str,
        ctx: &'data str,
        scoping: &'sema Scoping,
    ) -> Self {
        Self {
            alloc,
            scoping,
            ctx,
            await_depth: 0,
            jschanges: JsChanges::new(ident),
            module: SystemJsModule {
                has_tla: false,
                deps: HashMap::new_in(alloc),
                hoisted_idents: Vec::new_in(&alloc),
                hoisted_fns: Vec::new_in(&alloc),
            },
            fn_depth: 0,
            exported_symbols: HashMap::new_in(alloc),
            import_bindings: HashMap::new_in(alloc),
        }
    }

    // ---- symbol resolution helpers ----

    fn ref_symbol(&self, r: &ast::IdentifierReference<'data>) -> Option<SymbolId> {
        r.reference_id
            .get()
            .and_then(|id| self.scoping.get_reference(id).symbol_id())
    }

    fn module_export_name_symbol(&self, n: &ast::ModuleExportName<'data>) -> Option<SymbolId> {
        match n {
            ast::ModuleExportName::IdentifierReference(r) => self.ref_symbol(r),
            _ => None,
        }
    }

    fn record_export_symbol(&mut self, sym: SymbolId, external: NamedMapImported<'data>) {
        let alloc = self.alloc;
        self.exported_symbols
            .entry(sym)
            .or_insert_with(|| Vec::new_in(&alloc))
            .push(external);
    }

    /// a fresh arena copy of the name(s) `sym` is exported under, or `None` if it isn't a live export
    fn export_names_for(&self, sym: SymbolId) -> Option<Vec<'alloc, NamedMapImported<'data>>> {
        let existing = self.exported_symbols.get(&sym)?;
        let mut v = Vec::new_in(&self.alloc);
        for name in existing {
            v.push(*name);
        }
        Some(v)
    }

    fn dep_for<'a>(&'a mut self, source: &ast::StringLiteral<'data>) -> &'a mut SystemJsDep<'alloc, 'data> {
        let alloc = self.alloc;
        let idx = self.module.deps.len();
        self.module
            .deps
            .entry(source.value.as_str())
            .or_insert_with(|| SystemJsDep::new(alloc, idx, source.raw.unwrap().as_str()))
    }

    fn one_name(&self, name: NamedMapImported<'data>) -> Vec<'alloc, NamedMapImported<'data>> {
        let mut v = Vec::new_in(&self.alloc);
        v.push(name);
        v
    }

    // ---- top-level scan: imports first (to know import bindings), then exports ----

    fn scan_top_level(&mut self, body: &oxc::allocator::Vec<'data, ast::Statement<'data>>) {
        for stmt in body {
            if let ast::Statement::ImportDeclaration(import) = stmt {
                self.scan_import(import);
            }
        }
        for stmt in body {
            match stmt {
                ast::Statement::ExportNamedDeclaration(export) => self.scan_export_named(export),
                ast::Statement::ExportDefaultDeclaration(export) => self.scan_export_default(export),
                ast::Statement::ExportAllDeclaration(export) => self.scan_export_all(export),
                // Plain (non-exported) top-level declarations are hoisted into the declare scope
                // too. ESM keeps every top-level binding in one module scope; a relocated exported
                // function can reference any of them, so they must live in the same (outer) scope
                // rather than staying nested inside `execute`.
                ast::Statement::FunctionDeclaration(func) => {
                    self.hoist_function(func.span.start, func, None);
                }
                ast::Statement::ClassDeclaration(class) => self.hoist_class(class.span.start, class, &[]),
                // `using`/`await using` have disposal semantics a plain `var` hoist would drop, so
                // they are left in `execute` untouched (their bindings just aren't hoisted).
                ast::Statement::VariableDeclaration(var) if !var.kind.is_using() => {
                    self.hoist_var(var.span, var, false);
                }
                _ => {}
            }
        }
    }

    fn scan_import(&mut self, import: &ast::ImportDeclaration<'data>) {
        use ast::ImportDeclarationSpecifier as S;
        let source = import.source.value.as_str();
        {
            let dep = self.dep_for(&import.source);
            for spec in import.specifiers.iter().flatten() {
                match spec {
                    S::ImportDefaultSpecifier(s) => dep.default_imports.push(s.local.name.as_str()),
                    S::ImportSpecifier(s) => {
                        dep.named_imports.push(NamedMap::new_ident(&s.imported, s.local.name.as_str()));
                    }
                    S::ImportNamespaceSpecifier(s) => dep.star_imports.push(s.local.name.as_str()),
                }
            }
        }
        for spec in import.specifiers.iter().flatten() {
            let (local, binding) = match spec {
                S::ImportDefaultSpecifier(s) => (&s.local, ImportBinding::Default { source }),
                S::ImportSpecifier(s) => {
                    (&s.local, ImportBinding::Named { source, imported: NamedMapImported::new(&s.imported) })
                }
                S::ImportNamespaceSpecifier(s) => (&s.local, ImportBinding::Star { source }),
            };
            self.module.hoisted_idents.push(local.name.as_str());
            if let Some(sym) = local.symbol_id.get() {
                self.import_bindings.insert(sym, binding);
            }
        }
        self.jschanges.add(change!(import.span, Delete));
    }

    fn scan_export_named(&mut self, export: &ast::ExportNamedDeclaration<'data>) {
        if let Some(source) = &export.source {
            {
                let dep = self.dep_for(source);
                for spec in &export.specifiers {
                    dep.reexports.push(NamedMap::new(&spec.exported, &spec.local));
                }
            }
            self.jschanges.add(change!(export.span, Delete));
        } else if let Some(decl) = &export.declaration {
            self.scan_export_declaration(export.span, decl);
        } else {
            // `export { a, b as c };` — re-export of module bindings
            let mut names = Vec::new_in(&self.alloc);
            for spec in &export.specifiers {
                let external = NamedMapImported::new(&spec.exported);
                let sym = self.module_export_name_symbol(&spec.local);

                // if the local is an imported binding, route the re-export into that dep's setter
                // so it stays live, instead of exporting a stale value once in `execute`
                if let Some(binding) = sym.and_then(|s| self.import_bindings.get(&s).copied()) {
                    self.route_import_reexport(binding, external);
                    continue;
                }

                if let Some(sym) = sym {
                    self.record_export_symbol(sym, external);
                }
                names.push(NamedMap { local: NamedMapImported::new(&spec.local), external });
            }
            self.jschanges.add(change!(export.span, ExportGroup { names }));
        }
    }

    fn route_import_reexport(&mut self, binding: ImportBinding<'data>, external: NamedMapImported<'data>) {
        match binding {
            ImportBinding::Named { source, imported } => {
                if let Some(dep) = self.module.deps.get_mut(source) {
                    dep.reexports.push(NamedMap { local: imported, external });
                }
            }
            ImportBinding::Default { source } => {
                if let Some(dep) = self.module.deps.get_mut(source) {
                    dep.reexports.push(NamedMap { local: NamedMapImported::Ident("default"), external });
                }
            }
            ImportBinding::Star { source } => {
                if let Some(dep) = self.module.deps.get_mut(source) {
                    dep.star_ns_reexports.push(external);
                }
            }
        }
    }

    fn scan_export_declaration(&mut self, span: Span, decl: &ast::Declaration<'data>) {
        match decl {
            ast::Declaration::VariableDeclaration(var) => self.hoist_var(span, var, true),
            ast::Declaration::FunctionDeclaration(func) => {
                let id = func.id.as_ref().expect("function declaration has a name");
                self.hoist_function(span.start, func, Some(NamedMapImported::Ident(id.name.as_str())));
            }
            ast::Declaration::ClassDeclaration(class) => {
                let id = class.id.as_ref().expect("class declaration has a name");
                self.hoist_class(span.start, class, &[NamedMapImported::Ident(id.name.as_str())]);
            }
            _ => {}
        }
    }

    /// Hoist a top-level `var`/`let`/`const` declaration: every bound name becomes a `var` in the
    /// declare scope (via `hoisted_idents`) and the initializer is left in `execute` as a plain
    /// assignment (the keyword is stripped). When `exported`, each initializer is additionally
    /// wrapped so the assignment re-exports the live value.
    fn hoist_var(&mut self, span: Span, var: &ast::VariableDeclaration<'data>, exported: bool) {
        for (i, d) in var.declarations.iter().enumerate() {
            match &d.id {
                ast::BindingPattern::BindingIdentifier(id) => {
                    let name = id.name.as_str();
                    let name_start = id.span.start;
                    self.module.hoisted_idents.push(name);
                    if exported {
                        if let Some(sym) = id.symbol_id.get() {
                            self.record_export_symbol(sym, NamedMapImported::Ident(name));
                        }
                    }

                    if let Some(init) = &d.init {
                        if exported {
                            if i == 0 {
                                self.jschanges.add(change!(
                                    Span::new(span.start, name_start),
                                    ExportInitLeft { name: NamedMapImported::Ident(name) }
                                ));
                            } else {
                                self.jschanges.add(change!(
                                    Span::new(name_start, name_start),
                                    ExportAssignLeft { names: self.one_name(NamedMapImported::Ident(name)) }
                                ));
                            }
                            let end = init.span().end;
                            self.jschanges.add(change!(Span::new(end, end), CloseParen { count: 1 }));
                        } else if i == 0 {
                            // strip the `var`/`let`/`const` keyword; `name = init` remains as an
                            // assignment to the hoisted `var`. Later declarators are already
                            // `, name = init` after the comma, so they need no change.
                            self.jschanges.add(change!(Span::new(span.start, name_start), Delete));
                        }
                    } else if i == 0 {
                        self.jschanges.add(change!(Span::new(span.start, name_start), Delete));
                    }
                }
                pattern => {
                    // destructuring: `const { a, b: c } = o` / `[x, y] = arr`
                    let Some(init) = &d.init else { continue };
                    let pat_start = pattern.span().start;

                    let mut bindings = std::vec::Vec::new();
                    collect_pattern_bindings(pattern, &mut bindings);
                    let mut names = Vec::new_in(&self.alloc);
                    for b in bindings {
                        let name = b.name.as_str();
                        self.module.hoisted_idents.push(name);
                        if exported {
                            if let Some(sym) = b.symbol_id.get() {
                                self.record_export_symbol(sym, NamedMapImported::Ident(name));
                            }
                            names.push(NamedMap {
                                local: NamedMapImported::Ident(name),
                                external: NamedMapImported::Ident(name),
                            });
                        }
                    }

                    if i == 0 {
                        self.jschanges.add(change!(Span::new(span.start, pat_start), Delete));
                    }
                    // wrap the assignment in parens so a leading `{` isn't parsed as a block
                    self.jschanges.add(change!(Span::new(pat_start, pat_start), OpenParen));
                    let end = init.span().end;
                    if exported {
                        self.jschanges.add(change!(Span::new(end, end), PatternExports { names }));
                    } else {
                        self.jschanges.add(change!(Span::new(end, end), CloseParen { count: 1 }));
                    }
                }
            }
        }
    }

    /// Relocate a top-level function declaration into the declare scope (see [`HoistedFn`]). For an
    /// exported function, `prefix_start` is the start of the `export `/`export default ` keyword to
    /// strip and `exported` its export name; for a plain function, `prefix_start == func.span.start`
    /// (nothing to strip) and `exported` is `None`.
    fn hoist_function(&mut self, prefix_start: u32, func: &ast::Function<'data>, exported: Option<NamedMapImported<'data>>) {
        let id = func.id.as_ref().expect("function declaration has a name");
        self.module.hoisted_fns.push(HoistedFn {
            span: func.span,
            exported,
            local: id.name.as_str(),
        });
        if let Some(name) = exported {
            if let Some(sym) = id.symbol_id.get() {
                self.record_export_symbol(sym, name);
            }
        }
        if prefix_start < func.span.start {
            self.jschanges.add(change!(Span::new(prefix_start, func.span.start), Delete));
        }
    }

    /// Hoist a top-level class: its name becomes a `var` in the declare scope and the class body is
    /// left in `execute` rewritten from a declaration into an assignment (`C = class C {…}`), so it
    /// still evaluates in place but binds the hoisted `var`. `exported` holds each export name (empty
    /// for a plain class); `prefix_start` is the start of any `export ` keyword to strip.
    fn hoist_class(&mut self, prefix_start: u32, class: &ast::Class<'data>, exported: &[NamedMapImported<'data>]) {
        let id = class.id.as_ref().expect("class declaration has a name");
        let name = id.name.as_str();
        self.module.hoisted_idents.push(name);
        if !exported.is_empty() {
            if let Some(sym) = id.symbol_id.get() {
                for e in exported {
                    self.record_export_symbol(sym, *e);
                }
            }
        }
        if prefix_start < class.span.start {
            self.jschanges.add(change!(Span::new(prefix_start, class.span.start), Delete));
        }
        let mut names = Vec::new_in(&self.alloc);
        for e in exported {
            names.push(*e);
        }
        let count = names.len() as u32;
        self.jschanges.add(change!(
            Span::new(class.span.start, class.span.start),
            HoistAssignLeft { names, local: name }
        ));
        if count > 0 {
            self.jschanges.add(change!(Span::new(class.span.end, class.span.end), CloseParen { count }));
        }
    }

    fn scan_export_default(&mut self, export: &ast::ExportDefaultDeclaration<'data>) {
        use ast::ExportDefaultDeclarationKind as K;
        const DEFAULT: NamedMapImported<'static> = NamedMapImported::Ident("default");
        match &export.declaration {
            K::FunctionDeclaration(func) if func.id.is_some() => {
                self.hoist_function(export.span.start, func, Some(DEFAULT));
            }
            K::ClassDeclaration(class) if class.id.is_some() => {
                self.hoist_class(export.span.start, class, &[DEFAULT]);
            }
            _ => {
                // `export default <expr>;` (or anonymous fn/class expression)
                let decl_span = export.declaration.span();
                self.jschanges.add(change!(
                    Span::new(export.span.start, decl_span.start),
                    ExportInitLeft { name: DEFAULT }
                ));
                self.jschanges.add(change!(Span::new(decl_span.end, decl_span.end), CloseParen { count: 1 }));
            }
        }
    }

    fn scan_export_all(&mut self, export: &ast::ExportAllDeclaration<'data>) {
        {
            let dep = self.dep_for(&export.source);
            if let Some(exported) = &export.exported {
                dep.star_ns_reexports.push(NamedMapImported::new(exported));
            } else {
                dep.star_reexport = true;
            }
        }
        self.jschanges.add(change!(export.span, Delete));
    }

    /// consume the visitor into its collected changes and module metadata
    pub fn finish(self) -> (JsChanges<'alloc, 'data>, SystemJsModule<'alloc, 'data>) {
        (self.jschanges, self.module)
    }
}

impl<'data> Visit<'data> for Visitor<'_, 'data, '_> {
    fn visit_program(&mut self, it: &ast::Program<'data>) {
        self.scan_top_level(&it.body);
        walk::walk_program(self, it);
    }

    // ---- top-level await detection ----

    fn visit_variable_declaration(&mut self, it: &ast::VariableDeclaration<'data>) {
        if self.fn_depth == 0 && it.kind.is_await() {
            self.module.has_tla = true;
        }
        walk::walk_variable_declaration(self, it);
    }
    fn visit_for_of_statement(&mut self, it: &ast::ForOfStatement<'data>) {
        if self.fn_depth == 0 && it.r#await {
            self.module.has_tla = true;
        }
        walk::walk_for_of_statement(self, it);
    }
    fn visit_await_expression(&mut self, it: &ast::AwaitExpression<'data>) {
        if self.fn_depth == 0 {
            self.module.has_tla = true;
        }
        emit_await_wrap(&mut self.jschanges, self.ctx, self.await_depth, it.span);
        self.await_depth += 1;
        walk::walk_await_expression(self, it);
        self.await_depth -= 1;
    }

    fn visit_try_statement(&mut self, it: &ast::TryStatement<'data>) {
        emit_try_instrumentation(&mut self.jschanges, self.ctx, it);
        walk::walk_try_statement(self, it);
    }

    fn visit_function(&mut self, it: &ast::Function<'data>, flags: oxc::syntax::scope::ScopeFlags) {
        self.fn_depth += 1;
        walk::walk_function(self, it, flags);
        self.fn_depth -= 1;
    }
    fn visit_arrow_function_expression(&mut self, it: &ast::ArrowFunctionExpression<'data>) {
        self.fn_depth += 1;
        walk::walk_arrow_function_expression(self, it);
        self.fn_depth -= 1;
    }
    fn visit_static_block(&mut self, it: &ast::StaticBlock<'data>) {
        self.fn_depth += 1;
        walk::walk_static_block(self, it);
        self.fn_depth -= 1;
    }

    // ---- nestable rewrites ----

    fn visit_meta_property(&mut self, it: &ast::MetaProperty<'data>) {
        if it.meta.name.as_str() == "import" && it.property.name.as_str() == "meta" {
            self.jschanges.add(change!(it.span, ContextMeta));
        }
    }

    fn visit_import_expression(&mut self, it: &ast::ImportExpression<'data>) {
        // replace the `import` keyword (6 chars) with `{ident}_context.import`
        self.jschanges.add(change!(Span::new(it.span.start, it.span.start + 6), ContextImport));
        walk::walk_import_expression(self, it);
    }

    fn visit_identifier_reference(&mut self, it: &ast::IdentifierReference<'data>) {
        // free `__moduleName` -> `{ident}_context.id` (only when it isn't a real binding)
        if it.name.as_str() == "__moduleName" && self.ref_symbol(it).is_none() {
            self.jschanges.add(change!(it.span, ContextId));
        }
    }

    fn visit_assignment_expression(&mut self, it: &ast::AssignmentExpression<'data>) {
        match &it.left {
            // `x = rhs` where x is an exported local -> `_export("x", x = rhs)`
            ast::AssignmentTarget::AssignmentTargetIdentifier(id) => {
                if let Some(names) = self.ref_symbol(id).and_then(|s| self.export_names_for(s)) {
                    let count = names.len() as u32;
                    self.jschanges
                        .add(change!(Span::new(it.span.start, it.span.start), ExportAssignLeft { names }));
                    self.jschanges
                        .add(change!(Span::new(it.span.end, it.span.end), CloseParen { count }));
                }
            }
            // `[x] = rhs` / `({ x } = rhs)` -> `([x] = rhs, _export("x", x), …)`
            ast::AssignmentTarget::ArrayAssignmentTarget(_)
            | ast::AssignmentTarget::ObjectAssignmentTarget(_) => {
                let mut targets = std::vec::Vec::new();
                self.collect_assign_targets(&it.left, &mut targets);
                let mut names = Vec::new_in(&self.alloc);
                for (sym, local) in targets {
                    if let Some(existing) = self.exported_symbols.get(&sym) {
                        for external in existing {
                            names.push(NamedMap { local: NamedMapImported::Ident(local), external: *external });
                        }
                    }
                }
                if !names.is_empty() {
                    self.jschanges
                        .add(change!(Span::new(it.span.start, it.span.start), OpenParen));
                    self.jschanges
                        .add(change!(Span::new(it.span.end, it.span.end), PatternExports { names }));
                }
            }
            _ => {}
        }
        walk::walk_assignment_expression(self, it);
    }

    fn visit_update_expression(&mut self, it: &ast::UpdateExpression<'data>) {
        if let ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = &it.argument
            && let Some(names) = self.ref_symbol(id).and_then(|s| self.export_names_for(s))
        {
            self.jschanges.add(change!(
                it.span,
                ExportUpdate {
                    names,
                    local: id.name.as_str(),
                    increment: matches!(it.operator, UpdateOperator::Increment),
                    prefix: it.prefix,
                }
            ));
            // argument is a bare identifier — nothing nested to walk
            return;
        }
        walk::walk_update_expression(self, it);
    }
}

impl<'data> Visitor<'_, 'data, '_> {
    /// collect (symbol, local name) for every identifier target in a destructuring assignment
    fn collect_assign_targets(
        &self,
        t: &ast::AssignmentTarget<'data>,
        out: &mut std::vec::Vec<(SymbolId, &'data str)>,
    ) {
        use ast::AssignmentTarget as T;
        match t {
            T::AssignmentTargetIdentifier(id) => {
                if let Some(sym) = self.ref_symbol(id) {
                    out.push((sym, id.name.as_str()));
                }
            }
            T::ArrayAssignmentTarget(a) => {
                for el in a.elements.iter().flatten() {
                    self.collect_maybe_default(el, out);
                }
                if let Some(rest) = &a.rest {
                    self.collect_assign_targets(&rest.target, out);
                }
            }
            T::ObjectAssignmentTarget(o) => {
                for prop in &o.properties {
                    use ast::AssignmentTargetProperty as P;
                    match prop {
                        P::AssignmentTargetPropertyIdentifier(p) => {
                            if let Some(sym) = self.ref_symbol(&p.binding) {
                                out.push((sym, p.binding.name.as_str()));
                            }
                        }
                        P::AssignmentTargetPropertyProperty(p) => self.collect_maybe_default(&p.binding, out),
                    }
                }
                if let Some(rest) = &o.rest {
                    self.collect_assign_targets(&rest.target, out);
                }
            }
            _ => {} // member expressions etc. are not module bindings
        }
    }

    fn collect_maybe_default(
        &self,
        el: &ast::AssignmentTargetMaybeDefault<'data>,
        out: &mut std::vec::Vec<(SymbolId, &'data str)>,
    ) {
        if let ast::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(wd) = el {
            self.collect_assign_targets(&wd.binding, out);
        } else if let Some(t) = el.as_assignment_target() {
            self.collect_assign_targets(t, out);
        }
    }
}

/// Emit the two changes that wrap an `await` expression in `{ctx}.restore({ctx}.frame, <await>)`,
/// preserving the async context across the await's suspension. `depth` orders sibling closers so
/// nested awaits at the same offset (`await await x`) close deepest-first.
fn emit_await_wrap<'alloc, 'data>(
    changes: &mut JsChanges<'alloc, 'data>,
    ctx: &'data str,
    depth: u32,
    span: Span,
) {
    changes.add(change!(Span::new(span.start, span.start), AwaitSaveLeft { ctx, depth }));
    changes.add(change!(Span::new(span.end, span.end), AwaitCloseRight { depth }));
}

/// Finds whether a subtree contains an `await` *directly* (not inside a nested function/arrow, which
/// is a separate async context). Used to decide if a `try` needs frame-restoration instrumentation.
struct DirectAwaitFinder {
    found: bool,
}
impl<'a> Visit<'a> for DirectAwaitFinder {
    fn visit_await_expression(&mut self, _it: &ast::AwaitExpression<'a>) {
        self.found = true;
    }
    // don't descend into nested functions/arrows — their awaits belong to another async context
    fn visit_function(&mut self, _it: &ast::Function<'a>, _flags: oxc::syntax::scope::ScopeFlags) {}
    fn visit_arrow_function_expression(&mut self, _it: &ast::ArrowFunctionExpression<'a>) {}
}

fn block_has_direct_await(block: &ast::BlockStatement) -> bool {
    let mut f = DirectAwaitFinder { found: false };
    f.visit_block_statement(block);
    f.found
}

/// Emit the try-wrapper instrumentation for a `try` whose block/handler contains a native `await`.
/// A rejected `await` throws before its inline `restore(...)` runs, so the frame captured before the
/// `try` (into a block-scoped `{ctx}$t`) is reinstated at the top of each `catch`/`finally`. Trys
/// without a relevant await are left untouched (no overhead, no wrapper block).
fn emit_try_instrumentation<'alloc, 'data>(
    changes: &mut JsChanges<'alloc, 'data>,
    ctx: &'data str,
    it: &ast::TryStatement<'data>,
) {
    let block_await = block_has_direct_await(&it.block);
    let handler = it.handler.as_deref();
    let handler_await = handler.is_some_and(|h| block_has_direct_await(&h.body));
    if !(block_await || handler_await) {
        return;
    }

    // wrapper block capturing the pre-try frame, closed after the whole try statement
    changes.add(change!(Span::new(it.span.start, it.span.start), TryFrameOpen { ctx }));
    changes.add(change!(Span::new(it.span.end, it.span.end), TryFrameClose));

    // restore at catch entry — only awaits in the try *block* can reject into the catch
    if block_await {
        if let Some(h) = handler {
            let at = h.body.span.start + 1;
            changes.add(change!(Span::new(at, at), TryFrameRestore { ctx }));
        }
    }
    // restore at finally entry — awaits in either the block or the catch can land here
    if let Some(fin) = it.finalizer.as_deref() {
        let at = fin.span.start + 1;
        changes.add(change!(Span::new(at, at), TryFrameRestore { ctx }));
    }
}

/// A minimal visitor that ONLY wraps `await` expressions for async-context propagation — no ESM
/// import/export lowering, no hoisting. Used for the CommonJS path (`Rewriter::rewrite_awaits`),
/// whose output is fed to `new Function(...)` verbatim rather than lowered to a SystemJS module.
pub struct AwaitVisitor<'alloc, 'data> {
    ctx: &'data str,
    await_depth: u32,
    pub jschanges: JsChanges<'alloc, 'data>,
}

impl<'alloc, 'data> AwaitVisitor<'alloc, 'data> {
    pub fn new(ctx: &'data str) -> Self {
        Self {
            ctx,
            await_depth: 0,
            jschanges: JsChanges::new(ctx),
        }
    }

    /// consume the visitor into its collected changes
    pub fn finish(self) -> JsChanges<'alloc, 'data> {
        self.jschanges
    }
}

impl<'data> Visit<'data> for AwaitVisitor<'_, 'data> {
    fn visit_await_expression(&mut self, it: &ast::AwaitExpression<'data>) {
        emit_await_wrap(&mut self.jschanges, self.ctx, self.await_depth, it.span);
        self.await_depth += 1;
        walk::walk_await_expression(self, it);
        self.await_depth -= 1;
    }

    fn visit_try_statement(&mut self, it: &ast::TryStatement<'data>) {
        emit_try_instrumentation(&mut self.jschanges, self.ctx, it);
        walk::walk_try_statement(self, it);
    }
}

/// collect every bound identifier in a binding pattern (recursing object/array/rest/default patterns)
fn collect_pattern_bindings<'a, 'b>(
    pat: &'b ast::BindingPattern<'a>,
    out: &mut std::vec::Vec<&'b ast::BindingIdentifier<'a>>,
) {
    match pat {
        ast::BindingPattern::BindingIdentifier(id) => out.push(id),
        ast::BindingPattern::ObjectPattern(o) => {
            for prop in &o.properties {
                collect_pattern_bindings(&prop.value, out);
            }
            if let Some(rest) = &o.rest {
                collect_pattern_bindings(&rest.argument, out);
            }
        }
        ast::BindingPattern::ArrayPattern(a) => {
            for elem in a.elements.iter().flatten() {
                collect_pattern_bindings(elem, out);
            }
            if let Some(rest) = &a.rest {
                collect_pattern_bindings(&rest.argument, out);
            }
        }
        ast::BindingPattern::AssignmentPattern(a) => collect_pattern_bindings(&a.left, out),
    }
}
