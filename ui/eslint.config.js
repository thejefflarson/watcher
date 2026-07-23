// Accessibility floor (JEF-431). A focused gate: only the jsx-a11y ruleset runs
// here so the dense-table interaction model can't silently regress into
// mouse-only rows or unlabeled charts. The type-check gate stays `tsc --noEmit`;
// this adds the a11y lint alongside it (see the ui job in ci.yml).
//
// Plugin is eslint-plugin-jsx-a11y-x, a maintained flat-config-native fork of
// eslint-plugin-jsx-a11y: the upstream 6.10.2 peer-caps eslint at ^9 and has no
// eslint 10 release, whereas the fork supports eslint ^9 || ^10. Its
// `recommended` set is a superset of upstream's (same rules under a `jsx-a11y-x/`
// prefix, plus `label-has-for`), so the a11y coverage this gate enforces is
// unchanged. Migrated here as part of the eslint 9→10 bump (#102).
import jsxA11yX from "eslint-plugin-jsx-a11y-x";
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
    plugins: { "jsx-a11y-x": jsxA11yX, "react-hooks": reactHooks },
    languageOptions: {
      parser: tsParser,
      parserOptions: { ecmaFeatures: { jsx: true } },
    },
    rules: jsxA11yX.configs.recommended.rules,
  },
];
