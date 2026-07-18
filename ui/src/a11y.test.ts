import { describe, expect, it, vi } from "vitest";
import type { KeyboardEvent } from "react";
import { rowKeyActivate } from "./a11y";

// A minimal stand-in for the React keyboard event the handler reads.
const evt = (key: string) =>
  ({ key, preventDefault: vi.fn() }) as unknown as KeyboardEvent & {
    preventDefault: ReturnType<typeof vi.fn>;
  };

describe("rowKeyActivate", () => {
  it("activates on Enter and prevents default", () => {
    const fn = vi.fn();
    const e = evt("Enter");
    rowKeyActivate(fn)(e);
    expect(fn).toHaveBeenCalledOnce();
    expect(e.preventDefault).toHaveBeenCalledOnce();
  });

  it("activates on Space and prevents default (so the page doesn't scroll)", () => {
    const fn = vi.fn();
    const e = evt(" ");
    rowKeyActivate(fn)(e);
    expect(fn).toHaveBeenCalledOnce();
    expect(e.preventDefault).toHaveBeenCalledOnce();
  });

  it("ignores other keys and leaves default behavior intact", () => {
    const fn = vi.fn();
    const e = evt("Tab");
    rowKeyActivate(fn)(e);
    expect(fn).not.toHaveBeenCalled();
    expect(e.preventDefault).not.toHaveBeenCalled();
  });
});
