/**
 * Frame-integrity gate and cursor pacing for the live screencast.
 *
 * A torn JPEG must never reach the panel's img — decoders paint static /
 * colored streaks instead of failing — so the hook drops invalid payloads
 * and keeps the last good frame. Waypoint streams (~18ms apart) must flag
 * the cursor sample as streaming so the overlay positions it raw instead
 * of fighting the CSS glide.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import {
  CURSOR_STREAM_GAP_MS,
  isCursorStream,
  isIntactJpegFrame,
  useScreencast,
} from "./useScreencast";
import { CURSOR_EVENT, SCREENCAST_EVENT } from "../lib/ipc";

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(() => Promise.resolve(() => {})),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(() =>
    Promise.resolve({ attached: true, headless: false, windowMode: "offscreen" }),
  ),
}));

const b64 = (bytes: number[]) => btoa(String.fromCharCode(...bytes));

describe("isIntactJpegFrame", () => {
  it("accepts a well-formed JPEG", () => {
    // Minimal SOI … EOI.
    expect(isIntactJpegFrame(b64([0xff, 0xd8, 0xff, 0xd9]))).toBe(true);
    // A longer payload with JFIF-ish head bytes.
    expect(
      isIntactJpegFrame(b64([0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0xff, 0xd9])),
    ).toBe(true);
  });

  it("accepts JPEGs whose base64 ends with padding", () => {
    // 7 payload bytes -> trailing "==" quantum; 8 bytes -> trailing "=".
    expect(isIntactJpegFrame(b64([0xff, 0xd8, 0x00, 0x11, 0x22, 0xff, 0xd9]))).toBe(true);
    expect(isIntactJpegFrame(b64([0xff, 0xd8, 0x00, 0x11, 0x22, 0x33, 0xff, 0xd9]))).toBe(
      true,
    );
  });

  it("drops a frame truncated before EOI", () => {
    expect(isIntactJpegFrame(b64([0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10]))).toBe(false);
  });

  it("drops an empty payload", () => {
    expect(isIntactJpegFrame("")).toBe(false);
  });

  it("drops non-JPEG bytes, even with a JPEG tail", () => {
    // PNG magic with an EOI tail: SOI check must fail.
    expect(isIntactJpegFrame(b64([0x89, 0x50, 0x4e, 0x47, 0xff, 0xd9]))).toBe(false);
    // Plain text ("hello").
    expect(isIntactJpegFrame("aGVsbG8=")).toBe(false);
  });

  it("drops payloads that are not whole base64 quanta", () => {
    expect(isIntactJpegFrame("abc")).toBe(false);
    expect(isIntactJpegFrame("abcde")).toBe(false);
  });

  it("drops payloads that are not base64 at all", () => {
    expect(isIntactJpegFrame("!!!!")).toBe(false);
  });
});

describe("isCursorStream", () => {
  it("uses a 50ms stream gap", () => {
    expect(CURSOR_STREAM_GAP_MS).toBe(50);
  });

  it("treats waypoint cadence as a stream, pauses as isolated", () => {
    expect(isCursorStream(18)).toBe(true);
    expect(isCursorStream(50)).toBe(true);
    expect(isCursorStream(51)).toBe(false);
    expect(isCursorStream(5000)).toBe(false);
  });
});

describe("useScreencast frame handler", () => {
  const GOOD_JPEG = b64([0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0xff, 0xd9]);
  const POISONED = b64([0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10]); // torn: no EOI

  type FrameHandler = (event: { payload: { data: string; session_id: number } }) => void;
  type CursorHandler = (event: {
    payload: {
      x: number;
      y: number;
      kind: "move";
      viewport_width: number;
      viewport_height: number;
      session_id: number;
    };
  }) => void;

  beforeEach(async () => {
    const { listen } = await import("@tauri-apps/api/event");
    vi.mocked(listen).mockClear();
    // jsdom has no rAF: flush the frame pump synchronously.
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
      cb(0);
      return 1;
    });
    vi.stubGlobal("cancelAnimationFrame", () => {});
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  async function renderLiveHook() {
    const { listen } = await import("@tauri-apps/api/event");
    const listenMock = vi.mocked(listen);
    const { result } = renderHook(() => useScreencast(true, () => {}));
    // Flush the status invoke so the attach mirror arms the handlers.
    await act(async () => {});
    const frameHandler = listenMock.mock.calls.find(([name]) => name === SCREENCAST_EVENT)?.[1] as
      | FrameHandler
      | undefined;
    const cursorHandler = listenMock.mock.calls.find(([name]) => name === CURSOR_EVENT)?.[1] as
      | CursorHandler
      | undefined;
    if (!frameHandler || !cursorHandler) throw new Error("screencast listeners not registered");
    return { result, frameHandler, cursorHandler };
  }

  it("renders a well-formed frame", async () => {
    const { result, frameHandler } = await renderLiveHook();
    act(() => {
      frameHandler({ payload: { data: GOOD_JPEG, session_id: 9 } });
    });
    expect(result.current.frame).toBe(GOOD_JPEG);
  });

  it("drops a poisoned frame and keeps the last good one", async () => {
    const { result, frameHandler } = await renderLiveHook();
    act(() => {
      frameHandler({ payload: { data: GOOD_JPEG, session_id: 9 } });
    });
    expect(result.current.frame).toBe(GOOD_JPEG);
    act(() => {
      frameHandler({ payload: { data: POISONED, session_id: 9 } });
    });
    // The torn payload never reaches state: no static, no blank.
    expect(result.current.frame).toBe(GOOD_JPEG);
  });

  it("never paints garbage when the first frame is poisoned", async () => {
    const { result, frameHandler } = await renderLiveHook();
    act(() => {
      frameHandler({ payload: { data: POISONED, session_id: 9 } });
    });
    expect(result.current.frame).toBeNull();
  });

  it("flags rapid waypoints as streaming and isolated placements as glides", async () => {
    const { result, frameHandler, cursorHandler } = await renderLiveHook();
    act(() => {
      frameHandler({ payload: { data: GOOD_JPEG, session_id: 9 } });
    });
    const move = (x: number): Parameters<CursorHandler>[0] => ({
      payload: { x, y: 100, kind: "move", viewport_width: 1920, viewport_height: 1080, session_id: 9 },
    });
    // First placement after silence: glide.
    act(() => {
      cursorHandler(move(100));
    });
    expect(result.current.cursor?.streaming).toBe(false);
    // Immediate follow-up waypoint: raw stream.
    act(() => {
      cursorHandler(move(140));
    });
    expect(result.current.cursor?.streaming).toBe(true);
    // After a pause longer than the stream gap: glide again.
    await act(async () => {
      await new Promise(resolve => setTimeout(resolve, CURSOR_STREAM_GAP_MS + 10));
    });
    act(() => {
      cursorHandler(move(180));
    });
    expect(result.current.cursor?.streaming).toBe(false);
  });
});
