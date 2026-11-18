import { describe, expect, it } from 'vitest';
import { FakeSqlDatabase } from './helpers/fake-sql';

describe('FakeSqlDatabase helper', () => {
  it('creates tables idempotently and rejects unknown statements', () => {
    const db = new FakeSqlDatabase();
    expect(() => db.exec('CREATE TABLE IF NOT EXISTS browser_session (singleton INTEGER PRIMARY KEY)')).not.toThrow();
    expect(() => db.exec('SELECT nonsense FROM missing_table')).toThrow(/unsupported statement/);
    db.createTable('t', ['a']);
    db.createTable('t', ['b']);
    expect(db.table('t')!.columns).toEqual(['a']);
  });

  it('inserts, updates, selects, and deletes session/snapshot/asset rows', () => {
    const db = new FakeSqlDatabase();
    db.exec(
      'INSERT INTO browser_session (singleton, status, created_at, updated_at, expires_at, width, height) VALUES (1, ?, ?, ?, ?, ?, ?)',
      'active', 1, 2, 3, 1280, 720,
    );
    expect(db.exec<{ status: string }>('SELECT status, created_at, updated_at, expires_at, width, height FROM browser_session WHERE singleton = 1').toArray()[0].status).toBe('active');

    db.exec('UPDATE browser_session SET status = ?, updated_at = ?, expires_at = ? WHERE singleton = 1', 'closed', 5, 6);
    const row = db.exec<{ status: string; updated_at: number }>('SELECT status, created_at, updated_at, expires_at, width, height FROM browser_session WHERE singleton = 1').toArray()[0];
    expect(row.status).toBe('closed');

    db.exec('UPDATE browser_session SET updated_at = ?, expires_at = ? WHERE singleton = 1 AND status = ?', 7, 8, 'closed');
    db.exec('UPDATE browser_session SET updated_at = ?, expires_at = ? WHERE singleton = 1 AND status = ?', 9, 9, 'nonexistent-status');
    const lease = db.exec<{ updated_at: number }>('SELECT status, created_at, updated_at, expires_at, width, height FROM browser_session WHERE singleton = 1').toArray()[0];
    expect(lease.updated_at).toBe(7);

    const upsert = 'INSERT INTO browser_snapshot (singleton, snapshot_json) VALUES (1, ?) ON CONFLICT(singleton) DO UPDATE SET snapshot_json = excluded.snapshot_json';
    db.exec(upsert, '{"v":1}');
    db.exec(upsert, '{"v":2}');
    expect(db.exec<{ snapshot_json: string }>('SELECT snapshot_json FROM browser_snapshot WHERE singleton = 1').toArray()).toHaveLength(1);
    expect(db.exec<{ snapshot_json: string }>('SELECT snapshot_json FROM browser_snapshot WHERE singleton = 1').toArray()[0].snapshot_json).toBe('{"v":2}');
    db.exec('DELETE FROM browser_snapshot');
    expect(db.exec('SELECT snapshot_json FROM browser_snapshot WHERE singleton = 1').toArray()).toHaveLength(0);

    db.exec('DELETE FROM browser_asset WHERE name = ?', 'font:000000');
    db.exec('INSERT INTO browser_asset (name, chunk_index, chunk_text) VALUES (?, ?, ?)', 'font:000001', 1, 'b');
    db.exec('INSERT INTO browser_asset (name, chunk_index, chunk_text) VALUES (?, ?, ?)', 'font:000001', 0, 'a');
    db.exec('INSERT INTO browser_asset (name, chunk_index, chunk_text) VALUES (?, ?, ?)', 'initial-html', 0, '<html>');
    expect(db.exec<{ chunk_text: string }>('SELECT chunk_text FROM browser_asset WHERE name = ? ORDER BY chunk_index', 'font:000001').toArray().map((c) => c.chunk_text)).toEqual(['a', 'b']);
    expect(db.exec<{ name: string }>('SELECT DISTINCT name FROM browser_asset WHERE name LIKE ?', 'font:%').toArray()).toEqual([{ name: 'font:000001' }]);
    expect(db.exec<{ count: number }>("SELECT COUNT(DISTINCT name) AS count FROM browser_asset WHERE name LIKE 'font:%'").toArray()).toEqual([{ count: 1 }]);
    db.exec('DELETE FROM browser_asset WHERE name = ?', 'font:000001');
    db.exec('DELETE FROM browser_asset');
    expect(db.exec('SELECT chunk_text FROM browser_asset WHERE name = ? ORDER BY chunk_index', 'initial-html').toArray()).toHaveLength(0);
  });
});
