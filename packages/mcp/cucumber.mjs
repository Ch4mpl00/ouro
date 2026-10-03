// The executable specification (specs/README.md). A sentence nobody taught
// the harness is undefined, and strict mode (the default) fails the run on
// it: the specification must never pass on words that check nothing.
export default {
  paths: ["specs/*.feature"],
  import: ["specs/steps/**/*.ts"],
  format: ["progress"],
  strict: true,
};
