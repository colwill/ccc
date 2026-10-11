// the esm face of ccc's node probe - re-exports the one commonjs instance, so esm and commonjs code count into the same records
import probe from "./index.js";

export const { hit, fixture, bind, current, baggage, fromBaggage, flush } = probe;
export default probe;
