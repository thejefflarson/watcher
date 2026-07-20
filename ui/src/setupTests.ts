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

// vitest-axe 0.1.0 augments the legacy `Vi` global namespace, which vitest v4 no
// longer routes `expect(...)` through — so the matcher is registered at runtime
// but untyped. Augment vitest's own `Assertion` here to type `toHaveNoViolations`.
declare module "vitest" {
  // `T = any` mirrors vitest's own `Assertion<T = any>` so the augmentation
  // merges (identical type parameters). T is unused here; it belongs to the base.
  interface Assertion<T = any> extends AxeMatchers {}
  interface AsymmetricMatchersContaining extends AxeMatchers {}
}
