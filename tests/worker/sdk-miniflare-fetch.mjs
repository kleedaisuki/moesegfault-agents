// OpenAI SDK and Miniflare use different fetch realms. Encode multipart with
// the same native Request/FormData pair before crossing into workerd.
export async function dispatchSDKFetch(mf, input, init) {
  if (input === 'data:,') return new Response('');
  if (!(init?.body instanceof FormData)) return mf.dispatchFetch(input, init);
  const request = new Request(input, init);
  const body = Buffer.from(await request.arrayBuffer());
  return mf.dispatchFetch(request.url, { ...init, body, headers: request.headers });
}
