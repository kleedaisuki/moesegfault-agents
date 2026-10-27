import { createHash } from 'node:crypto';

export const deepseekModelsLimit = 256 * 1024;

// This is credential metadata, not inference or balance/account inspection.
export async function verifyDeepseekCredential(key, { fetch: request = globalThis.fetch,
  timeoutMs = 15_000, maxBytes = deepseekModelsLimit, report = () => {} } = {}) {
  const started = Date.now();
  const metadata = { http_status:null,request_id:null,duration_ms:0,response_bytes:0,response_sha256:null,error_code:null };
  const signal = AbortSignal.timeout(timeoutMs);
  const finish = code => {
    metadata.duration_ms = Date.now()-started;
    metadata.error_code = code;
    report({...metadata});
    if (code) throw Object.assign(new Error(code), {code,details:{...metadata}});
    return {...metadata};
  };
  if (typeof key !== 'string' || !key) return finish('deepseek_models_missing_key');
  let reader;
  try {
    const response = await request('https://api.deepseek.com/models', {method:'GET',redirect:'manual',
      signal,headers:{Authorization:`Bearer ${key}`,Accept:'application/json'}});
    metadata.http_status=response.status;
    metadata.request_id=[response.headers.get('x-request-id'),response.headers.get('cf-ray')]
      .find(id=>typeof id==='string'&&id.length<=256&&!id.includes(key)&&/^[A-Za-z0-9_.:/-]+$/.test(id))??null;
    report({...metadata});
    const chunks=[];
    reader=response.body?.getReader();
    if (reader) while (true) {
      const {done,value}=await reader.read();
      if (done) break;
      metadata.response_bytes+=value.byteLength;
      if (metadata.response_bytes>maxBytes) {
        void reader.cancel().catch(()=>{});
        return finish('deepseek_models_body_too_large');
      }
      chunks.push(Buffer.from(value));
    }
    const bytes=Buffer.concat(chunks);
    metadata.response_sha256=createHash('sha256').update(bytes).digest('hex');
    if (response.status!==200) return finish(response.status===401||response.status===403
      ?'deepseek_models_auth_rejected':'deepseek_models_http_rejected');
    let body;
    try {body=JSON.parse(new TextDecoder('utf-8',{fatal:true}).decode(bytes));}
    catch {return finish('deepseek_models_invalid_json');}
    if (body?.object!=='list'||!Array.isArray(body.data)||body.data.length===0
      ||!body.data.every(model=>typeof model?.id==='string'&&model.id.trim().length>0)) return finish('deepseek_models_invalid_list');
    return finish(null);
  } catch (error) {
    if (error?.details===undefined) return finish(signal.aborted?'deepseek_models_timeout':'deepseek_models_transport_failed');
    throw error;
  } finally {reader?.releaseLock();}
}
