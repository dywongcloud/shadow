const kEvents = Symbol('nodejs.event_target.events');
const kResistStopPropagation = Symbol('nodejs.event_target.resistStopPropagation');
const kWeakHandler = Symbol('nodejs.event_target.weakHandler');

function isEventTarget(value) {
  return !!value && typeof value.addEventListener === 'function' && typeof value.removeEventListener === 'function';
}

const Event = globalThis.Event;
const EventTarget = globalThis.EventTarget;

export {
  Event,
  EventTarget,
  kEvents,
  kResistStopPropagation,
  kWeakHandler,
  isEventTarget,
};

export default {
  Event,
  EventTarget,
  kEvents,
  kResistStopPropagation,
  kWeakHandler,
  isEventTarget,
};
