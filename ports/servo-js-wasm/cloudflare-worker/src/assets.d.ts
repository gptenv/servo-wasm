declare module '*.wasm' {
  const module: WebAssembly.Module;
  export default module;
}

declare module '*.mjs' {
  export function createServoWorkerRuntime(module: WebAssembly.Module, options?: Record<string, unknown>): Promise<any>;
}
