use js_sys::{Object, Reflect, Uint8Array};
use oxc::allocator::Allocator;
use rewriter::{Rewriter as CoreRewriter, RewriterError as CoreRewriterError};
use thiserror::Error;
use wasm_bindgen::prelude::*;

#[derive(Debug, Error)]
pub enum RewriterError {
	#[error("rewriter error: {0}")]
	Core(#[from] CoreRewriterError),
	#[error("str fromutf8 error: {0}")]
	Utf8(#[from] std::str::Utf8Error),
	#[error("reflect set failed: {0}")]
	ReflectSetFail(&'static str),
	#[error("js error: {0}")]
	Js(String),
}

impl From<JsValue> for RewriterError {
	fn from(value: JsValue) -> Self {
		Self::Js(js_sys::Error::from(value).to_string().into())
	}
}

impl From<RewriterError> for JsValue {
	fn from(value: RewriterError) -> Self {
		JsError::from(value).into()
	}
}

type Result<T> = std::result::Result<T, RewriterError>;

#[wasm_bindgen(typescript_custom_section)]
const REWRITER_OUTPUT: &'static str = r#"
export type JsRewriterOutput = {
    js: Uint8Array,
    errors: string[],
};
"#;

#[wasm_bindgen]
extern "C" {
	#[wasm_bindgen(typescript_type = "JsRewriterOutput")]
	pub type JsRewriterOutput;
}

fn set_obj(obj: &Object, k: &'static str, v: &JsValue) -> Result<()> {
	if Reflect::set(&obj.into(), &k.into(), v)? {
		Ok(())
	} else {
		Err(RewriterError::ReflectSetFail(k))
	}
}

fn build_output(result: &rewriter::RewriteResult<'_>) -> Result<JsRewriterOutput> {
	let obj = Object::new();

	set_obj(&obj, "js", &Uint8Array::from(result.js.as_slice()))?;
	set_obj(
		&obj,
		"errors",
		&result
			.errors
			.iter()
			.map(|e| JsValue::from(e.to_string()))
			.collect::<js_sys::Array>(),
	)?;

	Ok(obj.unchecked_into())
}

#[wasm_bindgen]
pub struct Rewriter {
	alloc: Allocator,
	inner: CoreRewriter,
}

#[wasm_bindgen]
impl Rewriter {
	#[wasm_bindgen(constructor)]
	#[must_use]
	pub fn new() -> Self {
		Self {
			alloc: Allocator::default(),
			inner: CoreRewriter::new(),
		}
	}

	#[wasm_bindgen]
	pub fn rewrite_js(&mut self, js: &str, ident: &str, ctx_global: &str) -> Result<JsRewriterOutput> {
		let result = self.inner.rewrite(&self.alloc, js, ident, ctx_global)?;
		let output = build_output(&result);
		self.alloc.reset();
		output
	}

	#[wasm_bindgen]
	pub fn rewrite_js_bytes(&mut self, js: &[u8], ident: &str, ctx_global: &str) -> Result<JsRewriterOutput> {
		let js = std::str::from_utf8(js)?;
		let result = self.inner.rewrite(&self.alloc, js, ident, ctx_global)?;
		let output = build_output(&result);
		self.alloc.reset();
		output
	}

	/// Await-only transform for CommonJS sources: wrap `await` for async-context propagation with no
	/// SystemJS lowering. `ctx_global` is the JS-owned holder-global name (same one passed to
	/// `rewrite_js`).
	#[wasm_bindgen]
	pub fn transform_awaits(&mut self, js: &str, ctx_global: &str) -> Result<JsRewriterOutput> {
		let result = self.inner.rewrite_awaits(&self.alloc, js, ctx_global)?;
		let output = build_output(&result);
		self.alloc.reset();
		output
	}
}

impl Default for Rewriter {
	fn default() -> Self {
		Self::new()
	}
}
