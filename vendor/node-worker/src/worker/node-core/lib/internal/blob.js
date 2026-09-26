function isBlob(value) {
  return typeof Blob !== 'undefined' && value instanceof Blob;
}

export { isBlob };

export default {
  isBlob,
};
