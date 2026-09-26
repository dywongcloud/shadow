class ReplHistory {
  constructor(context, options = {}) {
    this.context = context;
    this.history = options.history ?? context.history ?? [];
    this.size = options.size ?? context.historySize ?? 30;
    this.index = -1;
  }

  initialize(callback) {
    callback(null, this.context);
  }

  addHistory() {
    const line = this.context.line;
    if (!line || !line.trim()) {
      return line;
    }

    if (this.history[0] !== line) {
      this.history.unshift(line);
      if (this.history.length > this.size) {
        this.history.length = this.size;
      }
    }

    this.index = -1;
    this.context.emit?.('history', this.history);
    return line;
  }

  canNavigateToNext() {
    return this.index > -1 && this.history.length > 0;
  }

  navigateToNext(search = '') {
    if (!this.canNavigateToNext()) {
      return null;
    }

    this.index -= 1;
    return this.index === -1 ? search : this.history[this.index];
  }

  canNavigateToPrevious() {
    return this.history.length !== this.index + 1 && this.history.length > 0;
  }

  navigateToPrevious(search = '') {
    if (!this.canNavigateToPrevious()) {
      return null;
    }

    this.index += 1;
    return this.index >= this.history.length ? search : this.history[this.index];
  }
}

export { ReplHistory };

export default {
  ReplHistory,
};
