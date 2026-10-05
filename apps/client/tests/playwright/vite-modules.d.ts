// Specs reach into the running app with `import("/src/...")` inside
// `page.evaluate`. Those are URLs served by Vite in the browser, not modules
// TypeScript can resolve from the test project, so declare them as untyped.
declare module "/src/*";
declare module "/tests/*";
