// Shares the compiled Worker/real migrations fixture; no mocked JavaScript auth handler.
import assert from 'node:assert/strict';
import { createCipheriv, createHash, generateKeyPairSync, randomBytes, sign, verify } from 'node:crypto';

const hash = value => createHash('sha256').update(value).digest('base64url');

export function createIdentityFixture() {
  const client = generateKeyPairSync('ec', { namedCurve: 'prime256v1' });
  const unrelated = generateKeyPairSync('ec', { namedCurve: 'prime256v1' }).publicKey;
  const issuer = generateKeyPairSync('rsa', { modulusLength: 2048 });
  const privateJwk = { ...client.privateKey.export({ format: 'jwk' }), alg: 'ES256', kid: 'fixture-client-es256', use: 'sig' };
  for (const field of ['x', 'y', 'd']) assert.equal(Buffer.from(privateJwk[field], 'base64url').length, 32);
  const jwks = { keys: [{ ...issuer.publicKey.export({ format: 'jwk' }), alg: 'RS256', kid: 'fixture-issuer-rs256', use: 'sig' }] };
  const assertions = new Set();
  const grants = { authorization_code: 0, refresh_token: 0 };
  const calls = { token: 0, revocation: 0 };
  let pending;
  function token(use, nonce) {
    const now = Math.floor(Date.now() / 1000);
    const header = Buffer.from(JSON.stringify({ alg: 'RS256', kid: 'fixture-issuer-rs256', typ: 'JWT' })).toString('base64url');
    const claims = Buffer.from(JSON.stringify({ iss: 'https://identity.test', sub: 'client-assertion-subject', aud: 'test-dashboard', iat: now, exp: now + 3600, token_use: use, scope: 'openid', ...(nonce ? { nonce } : {}) })).toString('base64url');
    const input = `${header}.${claims}`;
    return `${input}.${sign('RSA-SHA256', Buffer.from(input), issuer.privateKey).toString('base64url')}`;
  }
  return {
    privateJwk, jwks, grants, calls,
    authorization(url) {
      const code = randomBytes(24).toString('base64url');
      pending = { code, nonce: url.searchParams.get('nonce'), challenge: url.searchParams.get('code_challenge') };
      assert.equal(url.searchParams.get('code_challenge_method'), 'S256');
      return code;
    },
    async handle(request) {
      const revoking = request.url === 'https://identity.test/revoke';
      if (!revoking) calls.token++;
      assert.ok(revoking || request.url === 'https://identity.test/token');
      assert.equal(request.method, 'POST');
      const form = new URLSearchParams(await request.text());
      assert.equal(form.get('client_id'), 'test-dashboard');
      assert.equal(form.get('client_assertion_type'), 'urn:ietf:params:oauth:client-assertion-type:jwt-bearer');
      const wire = form.get('client_assertion');
      assert.equal(typeof wire, 'string');
      const parts = wire.split('.');
      assert.equal(parts.length, 3);
      const header = JSON.parse(Buffer.from(parts[0], 'base64url'));
      const claims = JSON.parse(Buffer.from(parts[1], 'base64url'));
      assert.equal(header.alg, 'ES256');
      assert.equal(header.kid, privateJwk.kid);
      const signature = Buffer.from(parts[2], 'base64url');
      assert.equal(signature.length, 64, 'ES256 JWT signatures must be IEEE P1363 r||s, not ASN.1 DER');
      const input = Buffer.from(`${parts[0]}.${parts[1]}`);
      assert.ok(verify('sha256', input, { key: client.publicKey, dsaEncoding: 'ieee-p1363' }, signature), 'Client assertion must verify against its registered public key');
      assert.ok(!verify('sha256', input, { key: unrelated, dsaEncoding: 'ieee-p1363' }, signature), 'An unrelated registered key must not verify the assertion');
      assert.equal(claims.iss, 'test-dashboard');
      assert.equal(claims.sub, 'test-dashboard');
      assert.equal(claims.aud, 'https://identity.test/token');
      const now = Math.floor(Date.now() / 1000);
      assert.ok(Number.isSafeInteger(claims.iat) && Math.abs(claims.iat - now) <= 30);
      assert.ok(Number.isSafeInteger(claims.exp) && claims.exp > now && claims.exp <= now + 120);
      assert.equal(typeof claims.jti, 'string');
      assert.ok(claims.jti.length >= 20 && !assertions.has(claims.jti), 'Every exchange must get a fresh assertion ID');
      assertions.add(claims.jti);
      if (revoking) {
        assert.equal(form.get('token'), 'fixture-refresh-rotated');
        assert.equal(form.get('token_type_hint'), 'refresh_token');
        assert.equal(form.get('grant_type'), null);
        calls.revocation++;
        return new Response(null, { status: 200 });
      }
      const grant = form.get('grant_type');
      assert.ok(Object.hasOwn(grants, grant));
      grants[grant]++;
      if (grant === 'authorization_code') {
        assert.equal(form.get('code'), pending.code);
        assert.equal(hash(form.get('code_verifier')), pending.challenge);
        assert.equal(form.get('redirect_uri'), 'https://agents.test/auth/callback');
        const nonce = pending.nonce;
        pending = undefined;
        return Response.json({ id_token: token('id', nonce), access_token: token('access'), refresh_token: 'fixture-refresh-original', expires_in: 1 });
      }
      assert.equal(form.get('refresh_token'), 'fixture-refresh-original');
      return Response.json({ access_token: token('access'), refresh_token: 'fixture-refresh-rotated', expires_in: 3600 });
    },
  };
}

export async function runClientAssertionJourney({ mf, db, origin, identity }) {
  const login = await mf.dispatchFetch(`${origin}/auth/login`, { redirect: 'manual' });
  assert.equal(login.status, 302);
  const authorization = new URL(login.headers.get('location'));
  const code = identity.authorization(authorization);
  const callback = new URL(`${origin}/auth/callback`);
  callback.searchParams.set('state', authorization.searchParams.get('state'));
  callback.searchParams.set('iss', 'https://identity.test');
  callback.searchParams.set('code', code);
  const result = await mf.dispatchFetch(callback.toString(), { headers: { Cookie: login.headers.get('set-cookie').split(';')[0] }, redirect: 'manual' });
  assert.equal(result.status, 302, 'Compiled Rust callback must authenticate using its registered client key');
  const cookie = result.headers.getSetCookie().find(value => value.startsWith('__Host-magents_session=')).split(';')[0];
  const sessionHash = hash(cookie.slice(cookie.indexOf('=') + 1));
  assert.equal(identity.grants.authorization_code, 1);
  const refreshed = await mf.dispatchFetch(`${origin}/v1/models`, { headers: { Cookie: cookie, Origin: origin } });
  assert.equal(refreshed.status, 200, 'Actual AuthSession must use a valid fresh client assertion for refresh');
  assert.equal(identity.grants.refresh_token, 1);
  const persisted = await db.prepare('SELECT refresh_version FROM browser_sessions WHERE token_hash=?1').bind(sessionHash).first();
  assert.equal(persisted.refresh_version, 1);
  const again = await mf.dispatchFetch(`${origin}/v1/models`, { headers: { Cookie: cookie, Origin: origin } });
  assert.equal(again.status, 200);
  assert.equal(identity.grants.refresh_token, 1, 'Fresh rotated access credentials must not be exchanged again');
  const account = await mf.dispatchFetch(`${origin}/v1/account`, { headers: { Cookie: cookie, Origin: origin } });
  assert.equal(account.status, 200);
  const profile = await account.json();
  assert.equal(typeof profile.csrf_token, 'string');
  const logout = await mf.dispatchFetch(`${origin}/auth/logout`, {
    method: 'POST', headers: { Cookie: cookie, Origin: origin, 'X-CSRF-Token': profile.csrf_token },
  });
  assert.equal(logout.status, 200);
  assert.equal(identity.calls.revocation, 1, 'Logout must revoke the rotated refresh token with a fresh valid client assertion');
  assert.equal(await db.prepare('SELECT COUNT(*) AS count FROM browser_sessions WHERE token_hash=?1').bind(sessionHash).first('count'), 0);
  const loggedOut = await mf.dispatchFetch(`${origin}/v1/models`, { headers: { Cookie: cookie, Origin: origin } });
  assert.equal(loggedOut.status, 401);
}

export async function runInvalidClientKeyJourney({ mf, db, origin, identity, updatePrivateJwk }) {
  const sessionsBefore = await db.prepare('SELECT COUNT(*) AS count FROM browser_sessions').first('count');
  const tokenCallsBefore = identity.calls.token;
  try {
    await updatePrivateJwk({ ...identity.privateJwk, d: Buffer.alloc(32).toString('base64url') });
    const login = await mf.dispatchFetch(`${origin}/auth/login`, { redirect: 'manual' });
    assert.equal(login.status, 302);
    const authorization = new URL(login.headers.get('location'));
    const callback = new URL(`${origin}/auth/callback`);
    callback.searchParams.set('state', authorization.searchParams.get('state'));
    callback.searchParams.set('iss', 'https://identity.test');
    callback.searchParams.set('code', identity.authorization(authorization));
    const result = await mf.dispatchFetch(callback.toString(), { headers: { Cookie: login.headers.get('set-cookie').split(';')[0] }, redirect: 'manual' });
    assert.ok(result.status >= 400 && result.status < 600, 'An invalid P-256 scalar must fail closed, never authenticate');
    assert.equal(identity.calls.token, tokenCallsBefore, 'Invalid client signing material must never reach the token endpoint');
    const currentDb = await mf.getD1Database('DB');
    assert.equal(await currentDb.prepare('SELECT COUNT(*) AS count FROM browser_sessions').first('count'), sessionsBefore);
  } finally {
    await updatePrivateJwk(identity.privateJwk);
  }
}

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
  async function seedSession(token, expiresAt) {
    const nonce = randomBytes(12);
    const cipher = createCipheriv('aes-256-gcm', key, nonce);
    cipher.setAAD(Buffer.from(`session:${hash(token)}`));
    const secrets = Buffer.from(JSON.stringify({ csrf_token: csrf, oauth: {}, access_expires_at: now + 3600 }));
    const encrypted = Buffer.concat([Buffer.from([1]), nonce, cipher.update(secrets), cipher.final(), cipher.getAuthTag()]).toString('base64url');
    await db.prepare('INSERT INTO browser_sessions(token_hash,account_id,tokens_encrypted,csrf_hash,expires_at,created_at) VALUES(?,?,?,?,?,?)')
      .bind(hash(token), owner, encrypted, hash(csrf), expiresAt, now).run();
  }
  await seedSession(cookie, now + 3600);
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

  // Observe the actual alarm side effect, not merely request-time expiry rejection.
  // Treating an epoch as worker-rs' relative i64 offset would leave this row until 2083.
  const shortCookie = randomBytes(32).toString('base64url');
  const expiresAt = Math.floor(Date.now() / 1000) + 3;
  await seedSession(shortCookie, expiresAt);
  response = await mf.dispatchFetch(`${origin}/v1/api-keys`, {
    headers: { Cookie: `__Host-magents_session=${shortCookie}`, Origin: origin },
  });
  assert.equal(response.status, 200, 'Short-lived session must arm its real AuthSession alarm');
  const present = () => db.prepare('SELECT COUNT(*) AS count FROM browser_sessions WHERE token_hash=?').bind(hash(shortCookie)).first('count');
  assert.equal(await present(), 1);
  const limit = Date.now() + 5000;
  while (await present() && Date.now() < limit) await new Promise(resolve => setTimeout(resolve, 100));
  assert.equal(await present(), 0, 'AuthSession alarm must physically delete the expired row within five seconds');
  assert.ok(Date.now() >= expiresAt * 1000, 'GC must not run before the absolute session expiry');
}
