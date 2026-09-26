/**
 * The session badge must never lie about the window mode: an off-screen
 * headed session owns real windows (just none the user can see), so it
 * must not be labeled "headless".
 *
 * The agent cursor overlay maps synthetic pointer positions (page CSS
 * pixels) onto the rendered frame, and must never appear on a settled card.
 */
import { describe, expect, it, vi } from "vitest";
import { render } from "@testing-library/react";
import ScreencastCard, { contentRect, cursorOffset, sessionBadgeLabel } from "./ScreencastCard";
import type { AgentCursor } from "../lib/ipc";

describe("sessionBadgeLabel", () => {
  it("labels an off-screen headed session as headed, not headless", () => {
    expect(sessionBadgeLabel(true, "offscreen", null)).toBe("live · off-screen headed session");
  });

  it("keeps the headless wording for a true headless session", () => {
    expect(sessionBadgeLabel(true, "headless", null)).toBe("live · headless background session");
  });

  it("labels a visible headed session as direct control", () => {
    expect(sessionBadgeLabel(true, "headed", null)).toBe("live · headful, direct control");
  });

  it("labels settled cards from the capture, honestly on failure", () => {
    expect(sessionBadgeLabel(false, "offscreen", null)).toBe("final frame");
    expect(sessionBadgeLabel(false, "offscreen", false)).toBe("last live frame");
  });
});

describe("agent cursor overlay", () => {
  const cursor: AgentCursor = {
    x: 960,
    y: 540,
    kind: "move",
    viewport_width: 1920,
    viewport_height: 1080,
    session_id: 7,
  };

  it("maps viewport coordinates onto a letterboxed frame", () => {
    // 16:9 frame (1280x720) contained in a 640x400 box: full width, top-pinned.
    const rect = contentRect(640, 400, 1280, 720);
    expect(rect).toEqual({ x: 0, y: 0, width: 640, height: 360 });
    expect(cursorOffset(cursor, rect)).toEqual({ left: 320, top: 180 });
  });

  it("centers the content horizontally when the box is wider", () => {
    const rect = contentRect(800, 360, 1280, 720);
    expect(rect.x).toBeCloseTo(80);
    expect(rect.width).toBeCloseTo(640);
    expect(cursorOffset({ ...cursor, x: 0 }, rect).left).toBeCloseTo(80);
  });

  it("falls back to 16:9 before image metadata loads", () => {
    const rect = contentRect(640, 400, 0, 0);
    expect(rect).toEqual({ x: 0, y: 0, width: 640, height: 360 });
  });

  const baseProps = {
    frame: "aGVsbG8=",
    live: true,
    cursor: null as AgentCursor | null,
    headless: true,
    windowMode: "offscreen" as const,
    busy: false,
    finalUrl: null,
    pageTitle: null,
    anchorHost: null,
    finalFrameCaptured: null as boolean | null,
    onTakeControl: () => {},
    onRelease: () => {},
  };

  function mockImgBox(w: number, h: number, natW = 1280, natH = 720) {
    vi.stubGlobal(
      "ResizeObserver",
      class {
        observe() {}
        unobserve() {}
        disconnect() {}
      },
    );
    const proto = HTMLImageElement.prototype as unknown as Record<string, unknown>;
    Object.defineProperties(proto, {
      clientWidth: { configurable: true, get: () => w },
      clientHeight: { configurable: true, get: () => h },
      naturalWidth: { configurable: true, get: () => natW },
      naturalHeight: { configurable: true, get: () => natH },
    });
  }

  function restoreImgBox() {
    const proto = HTMLImageElement.prototype as unknown as Record<string, unknown>;
    for (const key of ["clientWidth", "clientHeight", "naturalWidth", "naturalHeight"]) {
      // eslint-disable-next-line @typescript-eslint/no-dynamic-delete
      delete proto[key];
    }
    vi.unstubAllGlobals();
  }

  it("shows the pointer on the live card at the scaled position", () => {
    mockImgBox(640, 360);
    try {
      const { container } = render(<ScreencastCard {...baseProps} cursor={cursor} />);
      const pointer = container.querySelector(".agent-cursor") as HTMLElement | null;
      expect(pointer).not.toBeNull();
      expect(pointer?.style.left).toBe("320px");
      expect(pointer?.style.top).toBe("180px");
    } finally {
      restoreImgBox();
    }
  });

  it("hides the pointer when there is no cursor", () => {
    const { container } = render(<ScreencastCard {...baseProps} cursor={null} />);
    expect(container.querySelector(".agent-cursor")).toBeNull();
  });

  it("never shows the pointer on a settled card", () => {
    mockImgBox(640, 360);
    try {
      const { container } = render(<ScreencastCard {...baseProps} cursor={cursor} live={false} />);
      expect(container.querySelector(".agent-cursor")).toBeNull();
    } finally {
      restoreImgBox();
    }
  });

  it("lands a ripple on press", () => {
    mockImgBox(640, 360);
    try {
      const press: AgentCursor = { ...cursor, kind: "press" };
      const { container } = render(<ScreencastCard {...baseProps} cursor={press} />);
      const ripple = container.querySelector(".cursor-ripple") as HTMLElement | null;
      expect(ripple).not.toBeNull();
      expect(ripple?.style.left).toBe("320px");
      expect(ripple?.style.top).toBe("180px");
    } finally {
      restoreImgBox();
    }
  });

  it("does not ripple on move or release", () => {
    mockImgBox(640, 360);
    try {
      const { container } = render(<ScreencastCard {...baseProps} cursor={cursor} />);
      expect(container.querySelector(".cursor-ripple")).toBeNull();
    } finally {
      restoreImgBox();
    }
  });
});
