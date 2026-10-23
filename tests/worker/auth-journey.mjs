// Shares the compiled Worker/real migrations fixture; no mocked JavaScript auth handler.
import assert from 'node:assert/strict';
import { createCipheriv, createHash, randomBytes } from 'node:crypto';

const hash = value => createHash('sha256').update(value).digest('base64url');

export async function runAuthJourney({ mf, db, encryptionKey, origin }) {
  const key = typeof encryptionKey === 'string' ? Buffer.from(encryptionKey, 'base64url') : encryptionKey;
  const now = Math.floor(Date.now() / 1000);
  const owner = `auth-life-${randomBytes(8).toString('hex')}`;
  const cookie = randomBytes(32).toString('base64url');
  const csrf = randomBytes(32).toString('base64url');
  await db.prepare('INSERT INTO accounts(id,issuer,subject,created_at,updated_at) VALUES(?,?,?,?,?)')
    .bind(owner, 'https://identity.test', owner, now, now).run();
  await db.prepare('INSERT INTO usage_accounts(account_id,budget_microusd,updated_at) VALUES(?,1000000,?)')
    .bind(owner, now).run();
  const nonce = randomBytes(12);
  const cipher = createCipheriv('aes-256-gcm', key, nonce);
  cipher.setAAD(Buffer.from(`session:${hash(cookie)}`));
  const secrets = Buffer.from(JSON.stringify({ csrf_token: csrf, oauth: {}, access_expires_at: now + 3600 }));
  const encrypted = Buffer.concat([Buffer.from([1]), nonce, cipher.update(secrets), cipher.final(), cipher.getAuthTag()]).toString('base64url');
  await db.prepare('INSERT INTO browser_sessions(token_hash,account_id,tokens_encrypted,csrf_hash,expires_at,created_at) VALUES(?,?,?,?,?,?)')
    .bind(hash(cookie), owner, encrypted, hash(csrf), now + 3600, now).run();
  const headers = { Cookie: `__Host-magents_session=${cookie}`, Origin: origin, 'X-CSRF-Token': csrf, 'Content-Type': 'application/json' };
  async function request(path, method, body, expected) {
    const response = await mf.dispatchFetch(`${origin}${path}`, {
      method, headers, ...(body === undefined ? {} : { body: JSON.stringify(body) }), redirect: 'manual',
    });
    // Error assertions deliberately never dump response bodies or credentials.
    assert.equal(response.status, expected, `${method} ${path.split('?')[0]}`);
    return response;
  }

  // Actual Rust AuthSession expiry query and actual timestamp writes cross the WASM ABI.
  let response = await request('/v1/api-keys', 'POST', { name: 'created-through-wasm' }, 201);
  const created = await response.json();
  let row = await db.prepare('SELECT created_at,typeof(created_at) AS kind FROM api_keys WHERE id=?').bind(created.id).first();
  assert.equal(row.kind, 'integer');
  assert.ok(row.created_at >= now && row.created_at <= now + 120);
  await request(`/v1/api-keys/${created.id}`, 'DELETE', undefined, 200);
  row = await db.prepare('SELECT revoked_at,typeof(revoked_at) AS kind FROM api_keys WHERE id=?').bind(created.id).first();
  assert.equal(row.kind, 'integer');
  assert.ok(row.revoked_at >= now);

  await request('/v1/provider-keys/deepseek', 'PUT', { api_key: 'synthetic-provider-key-only' }, 200);
  await request('/v1/provider-keys/deepseek', 'PUT', { api_key: 'rotated-synthetic-provider-key' }, 200);
  row = await db.prepare('SELECT typeof(created_at) AS created,typeof(updated_at) AS updated,key_version FROM provider_keys WHERE account_id=?').bind(owner).first();
  assert.deepEqual(row, { created: 'integer', updated: 'integer', key_version: 2 });

  // A denied provider callback still atomically consumes its server transaction.
  response = await request('/auth/login?return_to=/', 'GET', undefined, 302);
  const authorization = new URL(response.headers.get('location'));
  const state = authorization.searchParams.get('state');
  assert.ok(state);
  const loginCookie = response.headers.get('set-cookie').split(';')[0];
  row = await db.prepare('SELECT typeof(created_at) AS created,typeof(expires_at) AS expiry FROM oidc_transactions WHERE state_hash=?').bind(hash(state)).first();
  assert.deepEqual(row, { created: 'integer', expiry: 'integer' });
  const callback = new URL(`${origin}/auth/callback`);
  callback.searchParams.set('state', state);
  callback.searchParams.set('iss', 'https://identity.test');
  callback.searchParams.set('error', 'access_denied');
  response = await mf.dispatchFetch(callback.toString(), { headers: { Cookie: loginCookie }, redirect: 'manual' });
  assert.equal(response.status, 401);
  assert.equal(await db.prepare('SELECT COUNT(*) AS count FROM oidc_transactions WHERE state_hash=?').bind(hash(state)).first('count'), 0);

  await request('/auth/logout', 'POST', undefined, 200);
  assert.equal(await db.prepare('SELECT COUNT(*) AS count FROM browser_sessions WHERE account_id=?').bind(owner).first('count'), 0);
}
