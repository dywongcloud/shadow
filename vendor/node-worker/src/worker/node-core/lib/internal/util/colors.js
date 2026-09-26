// Override for upstream `internal/util/colors`. Console lazy-loads this and, on
// the first `console.log`, calls `shouldColorize(stream)` because the global
// console's colorMode is 'auto'. Upstream consults `internal/tty` for the color
// depth; this runtime has no such module, so we decide purely from the stream's
// TTY state (and honor FORCE_COLOR). Note the simplified `util.inspect` here
// ignores the resulting `colors` option anyway, so output stays plain either
// way — this exists so the lazy require doesn't throw.

function shouldColorize(stream) {
  if (typeof process !== 'undefined' && process.env && process.env.FORCE_COLOR !== undefined) {
    return process.env.FORCE_COLOR !== '0';
  }
  return !!(stream && stream.isTTY && (
    typeof stream.getColorDepth === 'function' ?
      stream.getColorDepth() > 2 : true));
}

// Kept as empty strings / disabled: nothing in this runtime emits these codes.
const colors = {
  blue: '',
  green: '',
  white: '',
  yellow: '',
  red: '',
  gray: '',
  clear: '',
  reset: '',
  hasColors: false,
  shouldColorize,
  refresh() {},
};

export { shouldColorize };

export default colors;
