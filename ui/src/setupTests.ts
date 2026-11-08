// Test setup shared by every vitest file (wired via `test.setupFiles` in
// vite.config.ts).
//
// - Registers vitest-axe's `toHaveNoViolations` matcher (the runtime a11y smoke
//   in *.a11y.test.tsx asserts with it).
// - Unmounts React trees between tests so a route's DOM can't leak into the next
//   scan (RTL's auto-cleanup only self-registers when vitest globals are on, and
//   this project imports its test helpers explicitly instead).
import { afterEach, expect } from "vitest";
import { cleanup } from "@testing-library/react";
import * as axeMatchers from "vitest-axe/matchers";
import type { AxeMatchers } from "vitest-axe/matchers";

expect.extend(axeMatchers);
afterEach(cleanup);

// vitest-axe 0.1.0 augments the legacy `Vi` global namespace, which vitest no
// longer routes `expect(...)` through — so the matcher is registered at runtime
// but untyped. Augment vitest's own `Assertion` here to type `toHaveNoViolations`.
declare module "vitest" {
  // The type parameters must be byte-identical to vitest's own
  // `Assertion<R extends void | Promise<void> = void, T = unknown>`, or
  // declaration merging fails with TS2428 ("all declarations must have identical
  // type parameters"). Neither is used here; both belong to the base interface.
  interface Assertion<R extends void | Promise<void> = void, T = unknown>
    extends AxeMatchers {}
  interface AsymmetricMatchersContaining extends AxeMatchers {}
}
