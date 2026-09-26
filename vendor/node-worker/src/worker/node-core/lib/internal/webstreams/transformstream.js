const TransformStream = globalThis.TransformStream;
const TransformStreamDefaultController = globalThis.TransformStreamDefaultController;

function isTransformStream(value) {
  return value instanceof TransformStream;
}

export {
  TransformStream,
  TransformStreamDefaultController,
  isTransformStream,
};

export default {
  TransformStream,
  TransformStreamDefaultController,
  isTransformStream,
};
