import h264EncoderFactory from './vendor/h264-mp4-encoder-aot.js';
import h264Mp4Wasm from './vendor/h264-mp4-encoder.wasm';

type H264Encoder = {
  outputFilename: string;
  width: number;
  height: number;
  frameRate: number;
  kbps: number;
  speed: number;
  quantizationParameter: number;
  groupOfPictures: number;
  initialize(): void;
  addFrameRgba(rgba: Uint8Array): void;
  finalize(): void;
  delete(): void;
};

type H264MP4Module = {
  H264MP4Encoder: new () => H264Encoder;
  FS: {
    open(path: string, flags?: string | number): unknown;
    stat(path: string): { size: number };
    read(stream: unknown, buffer: Uint8Array, offset: number, length: number, position: number): number;
    close(stream: unknown): void;
    unlink(path: string): void;
  };
};

type RecordingFrame = { slot: number; jpeg: Uint8Array };
const OUTPUT_CHUNK_BYTES = 512 * 1024;

let h264ModulePromise: Promise<H264MP4Module> | undefined;

function getH264Module(): Promise<H264MP4Module> {
  if (!h264ModulePromise) {
    h264ModulePromise = h264EncoderFactory({
      instantiateWasm(imports, receiveInstance) {
        receiveInstance(new WebAssembly.Instance(h264Mp4Wasm, imports));
        return {};
      },
    });
  }
  return h264ModulePromise;
}

let encoderQueue: Promise<void> = Promise.resolve();

async function encodeQueued(options: {
  width: number;
  height: number;
  fps: number;
  totalFrames: number;
  maxOutputBytes: number;
  nextFrame(afterSlot: number): Promise<RecordingFrame | undefined>;
  decode(jpeg: Uint8Array): Promise<{ width: number; height: number; rgba: Uint8Array }>;
  writeOutputChunk(chunkIndex: number, chunk: Uint8Array): void;
}): Promise<number> {
  if (options.totalFrames < 1) throw new Error('The recording contains no frames.');
  const module = await getH264Module();
  const encoder = new module.H264MP4Encoder();
  const filename = `servo-recording-${crypto.randomUUID()}.mp4`;
  encoder.outputFilename = filename;
  encoder.width = options.width;
  encoder.height = options.height;
  encoder.frameRate = options.fps;
  encoder.kbps = 500;
  encoder.speed = 10;
  encoder.quantizationParameter = 35;
  encoder.groupOfPictures = options.fps * 2;

  try {
    encoder.initialize();
    let nextOutputSlot = 0;
    let lastRgba: Uint8Array | undefined;
    let afterSlot = -1;
    while (true) {
      const frame = await options.nextFrame(afterSlot);
      if (!frame) break;
      if (frame.slot <= afterSlot || frame.slot >= options.totalFrames) {
        throw new Error('The recording frame sequence is invalid.');
      }
      while (nextOutputSlot < frame.slot) {
        if (!lastRgba) throw new Error('The recording is missing its first frame.');
        encoder.addFrameRgba(lastRgba);
        nextOutputSlot++;
      }
      const decoded = await options.decode(frame.jpeg);
      if (decoded.width !== options.width || decoded.height !== options.height) {
        throw new Error('Recording frame dimensions changed while encoding.');
      }
      encoder.addFrameRgba(decoded.rgba);
      lastRgba = decoded.rgba;
      nextOutputSlot = frame.slot + 1;
      afterSlot = frame.slot;
    }
    if (!lastRgba) throw new Error('The recording contains no decodable frames.');
    while (nextOutputSlot < options.totalFrames) {
      encoder.addFrameRgba(lastRgba);
      nextOutputSlot++;
    }
    encoder.finalize();
    const outputBytes = module.FS.stat(filename).size;
    if (!outputBytes) throw new Error('The MP4 encoder produced an empty file.');
    if (outputBytes > options.maxOutputBytes) {
      throw new RangeError(`The encoded MP4 exceeded ${options.maxOutputBytes} bytes.`);
    }
    // Emscripten's FS.readFile allocates another full-size JS buffer. Read the
    // finalized file through one reusable bounded buffer so output is flushed
    // to durable storage a chunk at a time.
    const stream = module.FS.open(filename, 'r');
    try {
      const buffer = new Uint8Array(Math.min(OUTPUT_CHUNK_BYTES, outputBytes));
      for (let offset = 0, chunkIndex = 0; offset < outputBytes; offset += OUTPUT_CHUNK_BYTES, chunkIndex++) {
        const length = Math.min(OUTPUT_CHUNK_BYTES, outputBytes - offset);
        const read = module.FS.read(stream, buffer, 0, length, offset);
        if (read !== length) throw new Error('The MP4 encoder returned a truncated output chunk.');
        options.writeOutputChunk(chunkIndex, buffer.subarray(0, length));
      }
    } finally {
      module.FS.close(stream);
    }
    return outputBytes;
  } finally {
    try { module.FS.unlink(filename); } catch {}
    encoder.delete();
  }
}

/** Serialize jobs because the encoder module shares one Emscripten FS per Worker isolate. */
export function encodeRecordingMp4(options: Parameters<typeof encodeQueued>[0]): Promise<number> {
  const result = encoderQueue.then(() => encodeQueued(options));
  encoderQueue = result.then(() => undefined, () => undefined);
  return result;
}
