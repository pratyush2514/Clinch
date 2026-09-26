/**
 * The session badge must never lie about the window mode: an off-screen
 * headed session owns real windows (just none the user can see), so it
 * must not be labeled "headless".
 */
import { describe, expect, it } from "vitest";
import { sessionBadgeLabel } from "./ScreencastCard";

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
