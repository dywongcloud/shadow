"use client";

// Virtual DVR — the recording engine behind the Time-Warner-style deck.
//
// The tenant's OWN browser and bandwidth do the work: the same <video> element
// already playing the stream is captured with captureStream() + MediaRecorder,
// so no fleet node ever proxies, transcodes or stores the feed mid-flight. The
// finished recording is uploaded to that project's Drive under `tv/` through
// the ordinary authenticated Drive API (PUT /v1/drive/:project/file), which
// means quota, sharing, WebDAV and deletion all work on a recording exactly
// like any other file the tenant owns.
//
// Scheduling is a real DVR's: a recording has a start (now, or a future clock
// time) and a duration, it survives channel-surfing within the session, and it
// stops itself on time. It does NOT survive a page close — an honest limit of
// browser-side capture, stated in the UI rather than papered over.

import { apiPutBytes } from "@/lib/api";

export type DvrState = "scheduled" | "recording" | "uploading" | "saved" | "failed";

export interface DvrRecording {
  id: string;
  project: string;
  region: string;
  channel: string;
  /** Epoch ms the recording should start (<= now means "already started"). */
  startAt: number;
  /** Requested length in ms. */
  durationMs: number;
  state: DvrState;
  /** Bytes written so far (post-stop for the final size). */
  bytes: number;
  /** Drive path once saved, or the failure reason. */
  path?: string;
  error?: string;
}

const MIME_CANDIDATES = [
  'video/webm;codecs="vp9,opus"',
  'video/webm;codecs="vp8,opus"',
  "video/webm",
  "video/mp4",
];

/** The first container this browser can actually record, or null. */
export function pickMime(): string | null {
  if (typeof MediaRecorder === "undefined") return null;
  for (const m of MIME_CANDIDATES) {
    try {
      if (MediaRecorder.isTypeSupported(m)) return m;
    } catch {
      /* older browsers throw instead of returning false */
    }
  }
  return null;
}

/** `tv/<region>/<channel>-<timestamp>.<ext>` — the Drive path a recording lands at. */
export function drivePath(rec: Pick<DvrRecording, "region" | "channel">, mime: string, at: number): string {
  const safe = (s: string) =>
    s
      .toLowerCase()
      .replace(/[^a-z0-9._-]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 48) || "channel";
  const d = new Date(at);
  const stamp = [
    d.getFullYear(),
    String(d.getMonth() + 1).padStart(2, "0"),
    String(d.getDate()).padStart(2, "0"),
    "-",
    String(d.getHours()).padStart(2, "0"),
    String(d.getMinutes()).padStart(2, "0"),
  ].join("");
  const ext = mime.startsWith("video/mp4") ? "mp4" : "webm";
  return `tv/${safe(rec.region)}/${safe(rec.channel)}-${stamp}.${ext}`;
}

export interface RecordHandle {
  stop: () => void;
}

/**
 * Capture `video` for `durationMs`, then upload to the project's Drive.
 * `onUpdate` is called on every state/byte change so the deck can render a
 * live progress bar. Returns a handle whose `stop()` ends the recording early
 * (the partial recording is still saved — a real DVR keeps what it got).
 */
export function startRecording(opts: {
  video: HTMLVideoElement;
  rec: DvrRecording;
  onUpdate: (patch: Partial<DvrRecording>) => void;
}): RecordHandle {
  const { video, rec, onUpdate } = opts;
  const mime = pickMime();
  if (!mime) {
    onUpdate({ state: "failed", error: "This browser cannot record video (no MediaRecorder support)." });
    return { stop: () => {} };
  }
  // captureStream is the whole trick: it taps the DECODED frames of the element
  // already playing, so the recording follows exactly what the viewer sees and
  // costs one extra encode — no second fetch of the upstream feed.
  const capture = (video as HTMLVideoElement & { captureStream?: () => MediaStream; mozCaptureStream?: () => MediaStream });
  const stream = capture.captureStream?.() ?? capture.mozCaptureStream?.();
  if (!stream) {
    onUpdate({ state: "failed", error: "This browser cannot capture the video element." });
    return { stop: () => {} };
  }

  let recorder: MediaRecorder;
  try {
    recorder = new MediaRecorder(stream, { mimeType: mime });
  } catch (e) {
    onUpdate({ state: "failed", error: `Recorder rejected the stream: ${String(e)}` });
    return { stop: () => {} };
  }

  const chunks: BlobPart[] = [];
  let bytes = 0;
  const startedAt = Date.now();

  recorder.ondataavailable = (ev) => {
    if (ev.data && ev.data.size > 0) {
      chunks.push(ev.data);
      bytes += ev.data.size;
      onUpdate({ bytes });
    }
  };

  recorder.onstop = async () => {
    onUpdate({ state: "uploading", bytes });
    try {
      const blob = new Blob(chunks, { type: mime });
      const path = drivePath(rec, mime, startedAt);
      await apiPutBytes(
        `/v1/drive/${encodeURIComponent(rec.project)}/file?path=${encodeURIComponent(path)}`,
        blob,
        mime,
      );
      onUpdate({ state: "saved", path, bytes: blob.size });
    } catch (e) {
      onUpdate({ state: "failed", error: String(e instanceof Error ? e.message : e) });
    }
  };

  // 1s timeslices so `bytes` ticks up live (and a crash keeps most of it).
  recorder.start(1000);
  onUpdate({ state: "recording", bytes: 0 });

  const timer = window.setTimeout(() => {
    if (recorder.state !== "inactive") recorder.stop();
  }, Math.max(1000, rec.durationMs));

  return {
    stop: () => {
      window.clearTimeout(timer);
      if (recorder.state !== "inactive") recorder.stop();
    },
  };
}

/** Human "1:04:00" / "22:30" for the deck's readouts. */
export function fmtClock(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  return h > 0
    ? `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}`
    : `${m}:${String(sec).padStart(2, "0")}`;
}

export function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}
