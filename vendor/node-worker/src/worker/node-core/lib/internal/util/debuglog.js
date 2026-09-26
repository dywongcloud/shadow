// `internal/util/debuglog` override. `debuglog` stays a no-op (this runtime has
// no NODE_DEBUG channels). The `time`/`timeEnd`/`timeLog`/`kNone` exports back
// `console.time`/`timeEnd`/`timeLog` — ported from upstream to plain JS. Trace
// events are no-ops here (see `internal-binding/trace_events`), so only the
// timing/logging behavior is meaningful.

// Read at call time, not here. This module is pulled in very early (the stream
// subgraph needs it) while the `internalBinding` table is a `const` object
// literal whose own dependencies sort it late, so a top-level call throws
// "Cannot access 'bindings' before initialization" — and which side wins shifts
// whenever the fs/stream subgraphs change shape. `trace` is a no-op here anyway.
const trace = (...args) => internalBinding('trace_events').trace(...args);

function debuglog(_section, callback = undefined) {
  const logger = () => {};
  if (typeof callback === 'function') {
    queueMicrotask(() => callback(logger));
  }
  return logger;
}

// Upstream `internal/constants` CHAR_LOWERCASE_{B,E,N}; only ever handed to the
// no-op `trace`, so the exact codes don't matter functionally.
const kTraceBegin = 98; // 'b'
const kTraceEnd = 101; // 'e'
const kTraceInstant = 110; // 'n'

const kNone = 1 << 0;
const kSkipLog = 1 << 1;
const kSkipTrace = 1 << 2;

const kSecond = 1000;
const kMinute = 60 * kSecond;
const kHour = 60 * kMinute;

function pad(value) {
  return `${value}`.padStart(2, '0');
}

function formatTime(ms) {
  let hours = 0;
  let minutes = 0;
  let seconds = 0;

  if (ms >= kSecond) {
    if (ms >= kMinute) {
      if (ms >= kHour) {
        hours = Math.floor(ms / kHour);
        ms = ms % kHour;
      }
      minutes = Math.floor(ms / kMinute);
      ms = ms % kMinute;
    }
    seconds = ms / kSecond;
  }

  if (hours !== 0 || minutes !== 0) {
    let msStr;
    [seconds, msStr] = seconds.toFixed(3).split('.', 2);
    const res = hours !== 0 ? `${hours}:${pad(minutes)}` : minutes;
    return `${res}:${pad(seconds)}.${msStr} (${hours !== 0 ? 'h:m' : ''}m:ss.mmm)`;
  }

  if (seconds !== 0) {
    return `${seconds.toFixed(3)}s`;
  }

  return `${Number(ms.toFixed(3))}ms`;
}

function safeTraceLabel(label) {
  return label.replaceAll('\\', '\\\\').replaceAll('"', '\\"');
}

function timeLogImpl(timesStore, implementation, logImp, label, args) {
  const time = timesStore.get(label);
  if (time === undefined) {
    process.emitWarning(`No such label '${label}' for ${implementation}`);
    return;
  }

  const duration = process.hrtime(time);
  const ms = duration[0] * 1000 + duration[1] / 1e6;

  const formatted = formatTime(ms);

  if (args === undefined) {
    logImp(label, formatted);
  } else {
    logImp(label, formatted, args);
  }
}

function time(timesStore, traceCategory, implementation, timerFlags, logLabel = 'default', traceLabel = undefined) {
  logLabel = `${logLabel}`;

  if (traceLabel !== undefined) {
    traceLabel = `${traceLabel}`;
  } else {
    traceLabel = logLabel;
  }

  if (timesStore.has(logLabel)) {
    process.emitWarning(`Label '${logLabel}' already exists for ${implementation}`);
    return;
  }

  if ((timerFlags & kSkipTrace) === 0) {
    traceLabel = safeTraceLabel(traceLabel);
    trace(kTraceBegin, traceCategory, traceLabel, 0);
  }

  timesStore.set(logLabel, process.hrtime());
}

function timeEnd(
  timesStore,
  traceCategory,
  implementation,
  timerFlags,
  logImpl,
  logLabel = 'default',
  traceLabel = undefined,
) {
  logLabel = `${logLabel}`;

  if (traceLabel !== undefined) {
    traceLabel = `${traceLabel}`;
  } else {
    traceLabel = logLabel;
  }

  if ((timerFlags & kSkipLog) === 0) {
    timeLogImpl(timesStore, implementation, logImpl, logLabel);
  }

  if ((timerFlags & kSkipTrace) === 0) {
    traceLabel = safeTraceLabel(traceLabel);
    trace(kTraceEnd, traceCategory, traceLabel, 0);
  }

  timesStore.delete(logLabel);
}

function timeLog(
  timesStore,
  traceCategory,
  implementation,
  timerFlags,
  logImpl,
  logLabel = 'default',
  traceLabel = undefined,
  args,
) {
  logLabel = `${logLabel}`;

  if (traceLabel !== undefined) {
    traceLabel = `${traceLabel}`;
  } else {
    traceLabel = logLabel;
  }

  if ((timerFlags & kSkipLog) === 0) {
    timeLogImpl(timesStore, implementation, logImpl, logLabel, args);
  }

  if ((timerFlags & kSkipTrace) === 0) {
    traceLabel = safeTraceLabel(traceLabel);
    trace(kTraceInstant, traceCategory, traceLabel, 0);
  }
}

export {
  debuglog,
  formatTime,
  kNone,
  kSkipLog,
  kSkipTrace,
  time,
  timeEnd,
  timeLog,
};

export default {
  debuglog,
  formatTime,
  kNone,
  kSkipLog,
  kSkipTrace,
  time,
  timeEnd,
  timeLog,
};
