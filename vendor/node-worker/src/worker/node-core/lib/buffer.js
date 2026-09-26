// re-export the npm 'buffer' polyfill. node/buffer.ts then re-exports this.
// keep this independent from node/buffer.ts so the two don't form a cycle.
import bufferModule from 'buffer';

export const Buffer = bufferModule.Buffer;
export const SlowBuffer = bufferModule.SlowBuffer;
export const INSPECT_MAX_BYTES = bufferModule.INSPECT_MAX_BYTES;
export const kMaxLength = bufferModule.kMaxLength;
export const Blob = bufferModule.Blob;
export const File = bufferModule.File;
export const constants = bufferModule.constants;

export default bufferModule;
