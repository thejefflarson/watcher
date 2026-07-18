import type { KeyboardEvent } from "react";

// Shared keyboard activation for elements that carry `role="button"` (the
// waterfall span rows, service-map nodes) — mirror a native button so Enter and
// Space fire the same action as a click. Space is prevented from scrolling.
export function rowKeyActivate(activate: () => void) {
  return (e: KeyboardEvent) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      activate();
    }
  };
}
