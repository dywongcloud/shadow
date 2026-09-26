use anyhow::{Context, Result};
use oxc::allocator::Allocator;
// `::rewriter` disambiguates the core crate from this `rewriter` module.
use ::rewriter::{RewriteResult, Rewriter};

/// Thin owner of the arena + core [`Rewriter`], mirroring scramjet's
/// `native/src/rewriter.rs`. Holds the allocator alive so the arena-backed
/// [`RewriteResult`] can borrow from it.
pub struct NativeRewriter {
	alloc: Allocator,
	rewriter: Rewriter,
}

impl NativeRewriter {
	pub fn new() -> Self {
		Self {
			alloc: Allocator::new(),
			rewriter: Rewriter::new(),
		}
	}

	pub fn rewrite<'a>(
		&'a self,
		data: &'a str,
		ident: &'a str,
		ctx_global: &'a str,
	) -> Result<RewriteResult<'a>> {
		self.rewriter
			.rewrite(&self.alloc, data, ident, ctx_global)
			.context("failed to rewrite file")
	}

	pub fn rewrite_awaits<'a>(
		&'a self,
		data: &'a str,
		ctx_global: &'a str,
	) -> Result<RewriteResult<'a>> {
		self.rewriter
			.rewrite_awaits(&self.alloc, data, ctx_global)
			.context("failed to rewrite file")
	}

	pub fn reset(&mut self) {
		self.alloc.reset();
	}
}

impl Default for NativeRewriter {
	fn default() -> Self {
		Self::new()
	}
}
