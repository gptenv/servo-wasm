import assert from 'node:assert/strict';
import test from 'node:test';
import { planServoRecordingCapture, planServoRecordingFinalization } from '../worker-adapter.mjs';

const recording = {
  status: 'recording',
  started_at: 1000,
  max_duration_ms: 2000,
  max_frames: 6,
  fps: 3,
};

test('capture timeline assigns slots without duplicates or skipped initial frame', () => {
  assert.deepEqual(planServoRecordingCapture(recording, 1000, true, null), {
    action: 'capture', slot: 0, deadline: 3000,
  });
  assert.deepEqual(planServoRecordingCapture(recording, 1090, false, 0), { action: 'skip' });
  assert.equal(planServoRecordingCapture(recording, 1400, false, 0).slot, 1);
  assert.deepEqual(planServoRecordingCapture(recording, 3000, false, 4), {
    action: 'finish', stoppedAt: 3000,
  });
});

test('finalization preserves the last recorded slot while bounding output', () => {
  assert.deepEqual(planServoRecordingFinalization(recording, 1400, 3), {
    end: 1400, targetFrames: 4,
  });
  assert.deepEqual(planServoRecordingFinalization(recording, 9000, 5), {
    end: 3000, targetFrames: 6,
  });
  assert.equal(planServoRecordingFinalization({ ...recording, status: 'failed' }, 2000), null);
});

test('non-recording sessions never capture frames', () => {
  assert.deepEqual(planServoRecordingCapture({ ...recording, status: 'encoding' }, 1400), {
    action: 'skip',
  });
});
