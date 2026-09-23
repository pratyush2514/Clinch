/**
 * jsdom gaps the thread relies on.
 *
 * The real webview is Chromium, so `showModal` and `scrollIntoView` exist
 * there. Stubbing them here keeps a missing layout API from being reported as a
 * component failure — the tests assert what is rendered, not how a native
 * dialog animates.
 */

import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";

if (typeof HTMLDialogElement !== "undefined") {
  if (!HTMLDialogElement.prototype.showModal) {
    HTMLDialogElement.prototype.showModal = function showModal(this: HTMLDialogElement) {
      this.open = true;
    };
  }
  if (!HTMLDialogElement.prototype.close) {
    HTMLDialogElement.prototype.close = function close(this: HTMLDialogElement) {
      this.open = false;
    };
  }
}

if (!Element.prototype.scrollIntoView) {
  Element.prototype.scrollIntoView = function scrollIntoView() {
    /* Layout is not under test. */
  };
}

afterEach(cleanup);
