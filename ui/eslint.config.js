// Accessibility floor (JEF-431). A focused gate: only eslint-plugin-jsx-a11y
// runs here so the dense-table interaction model can't silently regress into
// mouse-only rows or unlabeled charts. The type-check gate stays `tsc --noEmit`;
// this adds the a11y lint alongside it (see the ui job in ci.yml).
import jsxA11y from "eslint-plugin-jsx-a11y";
import reactHooks from "eslint-plugin-react-hooks";
import tsParser from "@typescript-eslint/parser";

export default [
  { ignores: ["dist", "node_modules"] },
  {
    files: ["src/**/*.{ts,tsx}"],
    // This gate's job is the jsx-a11y ruleset, nothing else: react-hooks is
    // registered (not enforced) only so pre-existing `eslint-disable
    // react-hooks/exhaustive-deps` directives resolve to a known rule, and
    // unused-directive reporting is off so those directives aren't flagged here.
    linterOptions: { reportUnusedDisableDirectives: "off" },
    plugins: { "jsx-a11y": jsxA11y, "react-hooks": reactHooks },
    languageOptions: {
      parser: tsParser,
      parserOptions: { ecmaFeatures: { jsx: true } },
    },
    rules: jsxA11y.flatConfigs.recommended.rules,
  },
];
