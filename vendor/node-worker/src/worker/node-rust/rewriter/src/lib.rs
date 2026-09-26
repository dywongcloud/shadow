mod changes;
mod visitor;

use oxc::{
	allocator::{Allocator, Vec as ArenaVec},
	ast_visit::Visit,
	diagnostics::OxcDiagnostic,
	parser::{ParseOptions, Parser},
	span::SourceType,
};
use thiserror::Error;

use visitor::{AwaitVisitor, Visitor};

#[derive(Debug, Error)]
pub enum RewriterError {
	#[error("transformer error: {0}")]
	Transformer(#[from] transform::TransformError),
}

pub struct RewriteResult<'alloc> {
	pub js: ArenaVec<'alloc, u8>,
	pub errors: Vec<OxcDiagnostic>,
}

pub struct Rewriter {}

impl Rewriter {
	#[must_use]
	pub fn new() -> Self {
		Self {}
	}

	pub fn rewrite<'a>(
		&self,
		alloc: &'a Allocator,
		js: &'a str,
		ident: &'a str,
		ctx_global: &'a str,
	) -> Result<RewriteResult<'a>, RewriterError> {
		let source_type = SourceType::unambiguous()
			.with_javascript(true)
			.with_standard(true)
			.with_module(true);

		let parsed = Parser::new(alloc, js, source_type)
			.with_options(ParseOptions {
				allow_v8_intrinsics: true,
				allow_return_outside_function: true,
				..Default::default()
			})
			.parse();

		// semantic analysis populates symbol/reference ids so live-binding rewrites can resolve
		// each assignment target to its binding (correct under shadowing)
		let semantic = oxc::semantic::SemanticBuilder::new().build(&parsed.program).semantic;

		let mut visitor = Visitor::new(alloc, ident, ctx_global, semantic.scoping());
		visitor.visit_program(&parsed.program);

		let (mut jschanges, module) = visitor.finish();
		let result = jschanges.perform(alloc, js, &module)?;

		Ok(RewriteResult {
			js: result,
			errors: parsed.diagnostics.into(),
		})
	}

	/// Await-only transform for CommonJS: wrap every `await` in `{ctx_global}.restore(...)` for
	/// async-context propagation, but perform NO SystemJS/ESM lowering — the source is parsed as a
	/// script and emitted verbatim except for the wraps. The result is meant to be run through the
	/// classic `new Function("require", "module", ...)` CJS harness.
	pub fn rewrite_awaits<'a>(
		&self,
		alloc: &'a Allocator,
		js: &'a str,
		ctx_global: &'a str,
	) -> Result<RewriteResult<'a>, RewriterError> {
		// Parse as a (non-strict) script so valid CJS constructs — top-level `return`, `with`, a
		// non-module `this` — don't trip module/strict parse errors. Awaits only occur inside async
		// functions here (CJS has no top-level await).
		let source_type = SourceType::unambiguous()
			.with_javascript(true)
			.with_standard(true)
			.with_module(false);

		let parsed = Parser::new(alloc, js, source_type)
			.with_options(ParseOptions {
				allow_v8_intrinsics: true,
				allow_return_outside_function: true,
				..Default::default()
			})
			.parse();

		let mut visitor = AwaitVisitor::new(ctx_global);
		visitor.visit_program(&parsed.program);

		let mut jschanges = visitor.finish();
		let result = jschanges.perform_remainder(alloc, js)?;

		Ok(RewriteResult {
			js: result,
			errors: parsed.diagnostics.into(),
		})
	}
}

impl Default for Rewriter {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(test)]
mod tests {
	use oxc::allocator::Allocator;

	use super::Rewriter;

	fn rw(js: &str) -> String {
		let alloc = Allocator::new();
		let res = Rewriter::new().rewrite(&alloc, js, "module", "actx").unwrap();
		std::str::from_utf8(&res.js).unwrap().to_string()
	}

	fn rw_cjs(js: &str) -> String {
		let alloc = Allocator::new();
		let res = Rewriter::new().rewrite_awaits(&alloc, js, "actx").unwrap();
		std::str::from_utf8(&res.js).unwrap().to_string()
	}

	#[test]
	fn wraps_in_system_register() {
		let out = rw("import { a } from \"x\";\nexport const b = a;\n");
		assert!(out.starts_with("module.register([\"x\"], function(module_export, module_context)"));
		assert!(out.contains("var a,b;"), "{out}");
		assert!(out.contains("a=module$0.a;"), "{out}");
		assert!(out.contains("module_export(\"b\", b = a)"), "{out}");
		assert!(out.trim_end().ends_with("}}})"), "{out}");
	}

	#[test]
	fn top_level_await_is_async_execute() {
		let out = rw("const x = await f();\n");
		assert!(out.contains("execute:async function()"), "{out}");
	}

	#[test]
	fn no_tla_is_sync_execute() {
		let out = rw("const x = 1;\nasync function g() { await x; }\n");
		assert!(out.contains("execute:function()"), "{out}");
	}

	#[test]
	fn live_binding_reassignment() {
		let out = rw("export let n = 0;\nn = 5;\n");
		assert!(out.contains("module_export(\"n\", n = 0)"), "{out}");
		assert!(out.contains("module_export(\"n\", n = 5)"), "{out}");
	}

	#[test]
	fn live_binding_multiple_names() {
		let out = rw("export let n = 0;\nexport { n as m };\nn = 1;\n");
		// reassignment must re-export under both names
		assert!(out.contains("module_export(\"n\", module_export(\"m\", n = 1))"), "{out}");
	}

	#[test]
	fn update_prefix_exports_new_value() {
		// `++x` value is the new value; a single wrap suffices
		let out = rw("export let n = 0;\nconst a = ++n;\n");
		assert!(out.contains("module_export(\"n\", ++n)"), "{out}");
	}

	#[test]
	fn update_postfix_preserves_old_value() {
		// `x++` must evaluate to the OLD value while exporting the NEW one (spec-correct via IIFE)
		let out = rw("export let n = 0;\nconst a = n++;\n");
		assert!(
			out.contains("(module$u => (module_export(\"n\", n), module$u))(n++)"),
			"{out}"
		);
	}

	#[test]
	fn shadowed_local_is_not_re_exported() {
		// the inner `value` is a distinct binding and must NOT be wrapped in _export
		let out = rw("export let value = 1;\nfunction f(value) { value = 2; return value; }\n");
		assert!(out.contains("module_export(\"value\", value = 1)"), "{out}");
		// exactly one _export("value" — the top-level init, not the shadowed assignment
		assert_eq!(out.matches("module_export(\"value\"").count(), 1, "{out}");
	}

	#[test]
	fn shadowed_block_scope_not_re_exported() {
		let out = rw("export let x = 1;\n{ let x = 9; x = 10; }\nx = 2;\n");
		// only the two module-scope writes (init + `x = 2`) are wrapped
		assert_eq!(out.matches("module_export(\"x\"").count(), 2, "{out}");
	}

	#[test]
	fn reexport_of_imported_binding_is_live() {
		// `import { a } from "m"; export { a as b };` must re-export inside m's setter, not execute
		let out = rw("import { a } from \"m\";\nexport { a as b };\n");
		assert!(out.contains("module_export(\"b\", module$0.a);"), "{out}");
	}

	// A try statement's end is a very common place for the next statement to start, and that next
	// statement often starts with an `await` — so the try wrapper's closing `}` and the await's
	// `restore(` opener share a `span.start`. Getting that tie-break backwards emitted the `}`
	// *inside* the `restore(` argument list. See `JsChangeType::rank`.
	#[test]
	fn try_wrapper_closes_before_a_following_await() {
		for src in [
			"async function f(){try{await a()}catch(x){}await c()}",
			"async function f(){try{await a()}finally{}await c()}",
			"async function f(){try{await a()}catch(x){}finally{}await c()}",
			"async function f(s,r,i,e){try{await s.write()}finally{await s.close()}await r.rename(i,e)}",
		] {
			let out = rw_cjs(src);
			// the wrapper block must close before the next await's wrap opens
			assert!(
				!out.contains("actx.restore(actx.frame, }"),
				"wrapper `}}` landed inside the restore() call: {out}"
			);
			assert!(out.contains("}actx.restore(actx.frame, await "), "{out}");
			assert_eq!(
				out.matches('{').count(),
				out.matches('}').count(),
				"unbalanced braces: {out}"
			);
			assert_eq!(
				out.matches('(').count(),
				out.matches(')').count(),
				"unbalanced parens: {out}"
			);
		}
	}

	// The inverse tie-break: an `await` that is the first token of a catch body shares its offset
	// with the `TryFrameRestore` opener, which must still come first. The try body needs an await of
	// its own, since that is what makes the frame restore necessary in the first place.
	#[test]
	fn catch_restore_precedes_an_await_at_the_same_offset() {
		let out = rw_cjs("async function f(){try{await g()}catch(e){await h()}}");
		let restore_frame = out.find("actx.frame=actx$t;").expect(&out);
		let await_wrap = out.find("actx.restore(actx.frame, await h()").expect(&out);
		assert!(restore_frame < await_wrap, "{out}");
	}

	#[test]
	fn reexport_of_imported_default_and_namespace() {
		let out = rw(
			"import d from \"m\";\nimport * as ns from \"n\";\nexport { d as x, ns as y };\n",
		);
		assert!(out.contains("module_export(\"x\", module$0.default);"), "{out}");
		assert!(out.contains("module_export(\"y\", module$1);"), "{out}");
	}

	#[test]
	fn string_named_export_is_escaped() {
		let out = rw("const v = 1;\nexport { v as \"a-b\" };\n");
		// the raw literal (with its quotes) is emitted verbatim — no double-quoting
		assert!(out.contains("module_export(\"a-b\", v);"), "{out}");
	}

	#[test]
	fn string_named_reexport_uses_bracket_access() {
		let out = rw("export { \"a-b\" as c } from \"m\";\n");
		assert!(out.contains("module_export(\"c\", module$0[\"a-b\"]);"), "{out}");
	}

	#[test]
	fn destructuring_assignment_reexports_targets() {
		let out = rw("export let a = 0, b = 0;\n[a, b] = arr;\n");
		assert!(
			out.contains("([a, b] = arr, module_export(\"a\", a), module_export(\"b\", b))"),
			"{out}"
		);
	}

	#[test]
	fn module_name_becomes_context_id() {
		let out = rw("console.log(__moduleName);\n");
		assert!(out.contains("console.log(module_context.id)"), "{out}");
	}

	#[test]
	fn shadowed_module_name_is_untouched() {
		let out = rw("function f(__moduleName) { return __moduleName; }\n");
		assert!(!out.contains("module_context.id"), "{out}");
	}

	#[test]
	fn destructuring_object_export() {
		let out = rw("export const { a, b: c } = o;\n");
		assert!(out.contains("var a,c;"), "{out}");
		assert!(
			out.contains("({ a, b: c } = o, module_export(\"a\", a), module_export(\"c\", c))"),
			"{out}"
		);
	}

	#[test]
	fn destructuring_array_export_with_rest() {
		let out = rw("export const [x, , y, ...rest] = arr;\n");
		assert!(out.contains("var x,y,rest;"), "{out}");
		assert!(
			out.contains("([x, , y, ...rest] = arr, module_export(\"x\", x), module_export(\"y\", y), module_export(\"rest\", rest))"),
			"{out}"
		);
	}

	#[test]
	fn hoists_exported_function_before_return() {
		let out = rw("console.log(1);\nexport function f(){ return 1; }\n");
		let reg = out.find("return{").unwrap();
		let f = out.find("function f()").unwrap();
		assert!(f < reg, "exported function should be hoisted before `return`: {out}");
		assert!(out.contains("module_export(\"f\", f);"), "{out}");
	}

	#[test]
	fn exported_class_hoisted_and_assigned_in_execute() {
		// the class name is hoisted to a `var` (so a relocated function could reference it) and the
		// declaration becomes an assignment in execute that also re-exports the class
		let out = rw("export class C {}\n");
		assert!(out.contains("var C;"), "{out}");
		let reg = out.find("return{").unwrap();
		let c = out.find("class C").unwrap();
		assert!(c > reg, "class body should evaluate in execute: {out}");
		assert!(out.contains("module_export(\"C\", C=class C {})"), "{out}");
	}

	#[test]
	fn export_default_expr() {
		let out = rw("export default 1 + 2;\n");
		assert!(out.contains("module_export(\"default\", 1 + 2)"), "{out}");
	}

	#[test]
	fn export_default_named_function_hoisted() {
		let out = rw("export default function main(){}\n");
		let reg = out.find("return{").unwrap();
		let m = out.find("function main()").unwrap();
		assert!(m < reg, "{out}");
		assert!(out.contains("module_export(\"default\", main);"), "{out}");
	}

	#[test]
	fn import_meta_and_dynamic_import() {
		let out = rw("const u = import.meta.url;\nconst m = import(\"y\");\n");
		assert!(out.contains("module_context.meta.url"), "{out}");
		assert!(out.contains("module_context.import(\"y\")"), "{out}");
	}

	#[test]
	fn export_star_and_ns_reexport() {
		let out = rw("export * from \"x\";\nexport * as ns from \"y\";\n");
		assert!(out.contains("for(var module$k in module$0)"), "{out}");
		assert!(out.contains("module_export(module$e)"), "{out}");
		assert!(out.contains("module_export(\"ns\", module$1);"), "{out}");
	}

	#[test]
	fn reexport_named_from_source() {
		let out = rw("export { a as b } from \"x\";\n");
		assert!(out.contains("module_export(\"b\", module$0.a);"), "{out}");
		// nothing left of the statement in the body
		assert!(out.contains("execute:function(){"), "{out}");
	}

	#[test]
	fn trailing_line_comment_does_not_swallow_footer() {
		// a source-map pragma (or any `//` line comment) with no trailing newline must
		// not eat the register footer — the `}}})` has to land on its own line
		let out = rw("export const a = 1;\n//# sourceMappingURL=x.js.map");
		assert!(!out.contains("x.js.map}}})"), "footer glued onto comment: {out}");
		assert!(out.contains("x.js.map\n}}})"), "{out}");
	}

	#[test]
	fn exported_fn_can_reference_nonexported_top_level() {
		// regression (chalk/supports-color): a relocated exported function must be able to call a
		// non-exported top-level function AND read a non-exported top-level const — all of which now
		// share the declare scope. Previously only the exported fn was hoisted, so it lost access to
		// its non-exported dependencies and threw a ReferenceError at runtime.
		let out = rw(
			"const secret = 1;\nfunction helper() { return secret; }\nexport function pub() { return helper(); }\n",
		);
		// the non-exported const's binding is hoisted; its initializer stays in execute as an assignment
		assert!(out.contains("var secret;"), "{out}");
		assert!(out.contains("secret = 1"), "const init should become an assignment: {out}");
		// both functions are relocated into the declare scope (before `return{`)
		let reg = out.find("return{").unwrap();
		assert!(out.find("function helper()").unwrap() < reg, "helper not hoisted: {out}");
		assert!(out.find("function pub()").unwrap() < reg, "pub not hoisted: {out}");
		// only the exported function is exported
		assert!(out.contains("module_export(\"pub\", pub);"), "{out}");
		assert!(!out.contains("module_export(\"helper\""), "helper must not be exported: {out}");
	}

	#[test]
	fn nonexported_class_is_hoisted() {
		let out = rw("class C {}\nexport function make() { return new C(); }\n");
		assert!(out.contains("var C;"), "{out}");
		// class becomes an assignment to the hoisted var, with no export
		assert!(out.contains("C=class C {}"), "{out}");
		assert!(!out.contains("module_export(\"C\""), "{out}");
		let reg = out.find("return{").unwrap();
		assert!(out.find("function make()").unwrap() < reg, "make not hoisted: {out}");
	}

	#[test]
	fn nonexported_destructuring_is_hoisted_and_parenthesized() {
		let out = rw("const { a, b } = obj;\nexport const c = a + b;\n");
		assert!(out.contains("var a,b,c;"), "{out}");
		// the destructuring assignment is wrapped so the leading `{` isn't parsed as a block
		assert!(out.contains("({ a, b } = obj)"), "{out}");
	}

	// ---- async-context await wrapping ----

	#[test]
	fn wraps_await_expression() {
		let out = rw("async function f(){ const x = await g(); return x; }\n");
		assert!(out.contains("actx.restore(actx.frame, await g())"), "{out}");
	}

	#[test]
	fn wraps_top_level_await_and_is_async_execute() {
		let out = rw("const x = await f();\n");
		assert!(out.contains("actx.restore(actx.frame, await f())"), "{out}");
		assert!(out.contains("execute:async function()"), "{out}");
	}

	#[test]
	fn wraps_nested_await_innermost_first() {
		let out = rw("async function f(){ return await g(await h()); }\n");
		// inner await wrapped as an argument, outer wrapped around the whole call
		assert!(
			out.contains("actx.restore(actx.frame, await g(actx.restore(actx.frame, await h())))"),
			"{out}"
		);
	}

	#[test]
	fn wraps_double_await_ordered() {
		// `await await x` — the closers share an offset and must nest deepest-first
		let out = rw("async function f(){ return await await x; }\n");
		assert!(
			out.contains("actx.restore(actx.frame, await actx.restore(actx.frame, await x))"),
			"{out}"
		);
	}

	#[test]
	fn wraps_await_in_exported_initializer() {
		// the await wrap must nest inside the live-binding `_export(...)` wrap
		let out = rw("export const v = await f();\n");
		assert!(
			out.contains("module_export(\"v\", v = actx.restore(actx.frame, await f()))"),
			"{out}"
		);
	}

	#[test]
	fn cjs_await_only_has_no_register_wrapper() {
		let out = rw_cjs("async function f(){ const x = await g(); return x; }\n");
		assert!(!out.contains(".register("), "cjs pass must not lower to systemjs: {out}");
		assert!(out.contains("actx.restore(actx.frame, await g())"), "{out}");
	}

	#[test]
	fn cjs_await_only_preserves_top_level_return() {
		// top-level return is valid in a CJS module wrapper; must parse as a script
		let out = rw_cjs("if (x) { return; }\nmodule.exports = 1;\n");
		assert!(out.contains("module.exports = 1"), "{out}");
	}

	// ---- try/catch/finally frame restoration (rejected-await path) ----

	#[test]
	fn instruments_try_catch_with_await() {
		let out = rw("async function f(){ try { const v = await g(); } catch (e) { h(e); } }\n");
		assert!(out.contains("{let actx$t=actx.frame;try"), "wrapper+capture: {out}");
		assert!(out.contains("catch (e) {actx.frame=actx$t;"), "catch restore: {out}");
		assert!(out.contains("actx.restore(actx.frame, await g())"), "{out}");
	}

	#[test]
	fn instruments_try_finally_with_await() {
		let out = rw("async function f(){ try { await g(); } finally { cleanup(); } }\n");
		assert!(out.contains("{let actx$t=actx.frame;try"), "{out}");
		assert!(out.contains("finally {actx.frame=actx$t;"), "finally restore: {out}");
	}

	#[test]
	fn try_without_await_is_not_instrumented() {
		let out = rw("async function f(){ try { g(); } catch (e) { h(e); } }\n");
		assert!(!out.contains("actx$t"), "no instrumentation without await: {out}");
	}

	#[test]
	fn nested_function_await_does_not_instrument_outer_try() {
		// the await is in a nested (non-async-boundary-sharing) function, so the try needs nothing
		let out = rw("async function f(){ try { const g = async () => await h(); } catch (e) {} }\n");
		assert!(!out.contains("actx$t"), "{out}");
	}

	#[test]
	fn cjs_instruments_try_catch_with_await() {
		let out = rw_cjs("async function f(){ try { await g(); } catch (e) { h(e); } }\n");
		assert!(out.contains("{let actx$t=actx.frame;try"), "{out}");
		assert!(out.contains("catch (e) {actx.frame=actx$t;"), "{out}");
	}
}
