/**
 * Minimal stand-ins for the Cloudflare Durable Object host APIs used by
 * `ServoBrowserSession`, built on top of {@link FakeSqlDatabase}.
 */

import { FakeSqlDatabase } from './fake-sql';

export interface AlarmSpy {
  calls: number[];
}

export interface FakeDurableObjectState {
  id: { toString(): string };
  storage: {
    sql: FakeSqlDatabase;
    transactionSync<T>(callback: () => T): T;
    getAlarm(): Promise<number | null>;
    setAlarm(at: number | Date): Promise<void>;
    deleteAlarm(): Promise<void>;
  };
  alarms: { set: (number | Date)[]; deleted: number; current: number | null };
  blockConcurrencyWhile<T>(callback: () => Promise<T>): Promise<T>;
  waitUntil(): void;
}

export function createFakeDurableObjectState(idString = 'session-do-id'): FakeDurableObjectState {
  const sql = new FakeSqlDatabase();
  sql.createTable('browser_session', ['singleton', 'status', 'created_at', 'updated_at', 'expires_at', 'width', 'height']);
  sql.createTable('browser_snapshot', ['singleton', 'snapshot_json']);
  sql.createTable('browser_asset', ['name', 'chunk_index', 'chunk_text']);
  sql.createTable('browser_recording', [
    'id', 'status', 'started_at', 'stopped_at', 'expires_at', 'fps', 'max_duration_ms',
    'max_frames', 'target_frames', 'captured_frames', 'stored_bytes', 'width', 'height',
    'download_token', 'error',
  ]);
  sql.createTable('browser_recording_frame', ['recording_id', 'slot_index', 'captured_at', 'jpeg_blob']);
  sql.createTable('browser_recording_output', ['recording_id', 'chunk_index', 'chunk_blob']);
  const alarms = { set: [] as (number | Date)[], deleted: 0, current: null as number | null };
  return {
    id: { toString: () => idString },
    storage: {
      sql,
      transactionSync: <T>(callback: () => T) => sql.transactionSync(callback),
      getAlarm: async () => alarms.current,
      setAlarm: async (at: number | Date) => {
        alarms.set.push(at);
        alarms.current = at instanceof Date ? at.getTime() : at;
      },
      deleteAlarm: async () => { alarms.deleted += 1; alarms.current = null; },
    },
    alarms,
    blockConcurrencyWhile: <T>(callback: () => Promise<T>) => callback(),
    waitUntil: () => undefined,
  } as FakeDurableObjectState & { alarms: typeof alarms };
}

/** Cast helper so tests can hand the fake state to the DO constructor. */
export function asDoState(fake: FakeDurableObjectState): DurableObjectState {
  return fake as unknown as DurableObjectState;
}
