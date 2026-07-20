/// <reference types="vitest/config" />
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  server: { port: 5173 },
  // jsdom gives the runtime axe smoke (src/*.a11y.test.tsx) a DOM to mount the
  // routes into. setupTests wires the vitest-axe matcher + RTL cleanup.
  test: {
    environment: "jsdom",
    setupFiles: ["./src/setupTests.ts"],
  },
});
