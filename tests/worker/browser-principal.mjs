import assert from 'node:assert/strict';
import { createCipheriv, createHash, randomBytes } from 'node:crypto';

const hash = value => createHash('sha256').update(value).digest('base64url');

export async function createBrowserPrincipal({ mf, db, encryptionKey, owner, origin = 'https://agents.test', oauth = {} }) {
  const cookie = randomBytes(32).toString('base64url');
  const csrf = randomBytes(32).toString('base64url');
  const now = Math.floor(Date.now() / 1000);
  const nonce = randomBytes(12);
  const cipher = createCipheriv('aes-256-gcm', encryptionKey, nonce);
  cipher.setAAD(Buffer.from(`session:${hash(cookie)}`));
  const encrypted = Buffer.concat([Buffer.from([1]), nonce,
    cipher.update(Buffer.from(JSON.stringify({ csrf_token: csrf, oauth, access_expires_at: now + 3600 }))),
    cipher.final(), cipher.getAuthTag()]).toString('base64url');
  await db.batch([
    db.prepare('INSERT INTO accounts(id,issuer,subject,created_at,updated_at) VALUES(?,?,?,?,?)').bind(owner, 'https://identity.test', owner, now, now),
    db.prepare('INSERT INTO usage_accounts(account_id,updated_at) VALUES(?,?)').bind(owner, now),
    db.prepare('INSERT INTO browser_sessions(token_hash,account_id,tokens_encrypted,csrf_hash,expires_at,created_at) VALUES(?,?,?,?,?,?)')
      .bind(hash(cookie), owner, encrypted, hash(csrf), now + 3600, now),
  ]);
  async function browser(method, endpoint, body, expected = 200) {
    const response = await mf.dispatchFetch(`${origin}${endpoint}`, {
      method, headers: { Cookie: `__Host-magents_session=${cookie}`, Origin: origin, 'X-CSRF-Token': csrf, 'Content-Type': 'application/json' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    assert.equal(response.status, expected, `${method} ${endpoint}`);
    const text = await response.text();
    return text ? JSON.parse(text) : null;
  }
  const apiKey = (await browser('POST', '/v1/api-keys', { name: 'Synthetic integration fixture' }, 201)).key;
  return { owner, apiKey, browser, cookie: `__Host-magents_session=${cookie}` };
}
