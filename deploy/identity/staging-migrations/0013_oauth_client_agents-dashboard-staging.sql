-- Reviewed staging-only shared-sector ownership exception: both hosts are user-owned.
-- Explicitly authorized 2026-10-23; shared billing subjects are intentional. Generic generator guard remains unchanged.
-- Deployment-owned OAuth client; public metadata only. / 部署所有的 OAuth 客户端，仅含公开元数据。
-- Apply as a reviewed forward-only D1 migration, not as separate CLI statements. / 作为已评审的只向前 D1 migration 应用，不得逐条执行。
INSERT INTO oauth_clients(client_id,display_name,client_type,token_endpoint_auth_method,sector_identifier,subject_salt_revision,state,created_at,updated_at) VALUES('agents-dashboard-staging','moeSegFault Agents dashboard (staging)','confidential','private_key_jwt','subscribe-staging.moesegfault.dev',1,'enabled',unixepoch(),unixepoch());
INSERT INTO oauth_redirect_uris(redirect_uri_id,client_id,redirect_uri,match_mode,created_at) VALUES('01a1691d-24c4-7fc4-983d-bb36138e0c88','agents-dashboard-staging','https://adash-staging.moesegfault.dev/auth/callback','exact',unixepoch());
INSERT INTO oauth_post_logout_redirect_uris(post_logout_redirect_uri_id,client_id,redirect_uri,created_at) VALUES('01a1691d-24c4-71d6-9791-07f5fa919905','agents-dashboard-staging','https://adash-staging.moesegfault.dev/',unixepoch());
INSERT INTO oauth_client_scopes(client_id,scope,granted_at) VALUES('agents-dashboard-staging','openid',unixepoch());
INSERT INTO oauth_client_scopes(client_id,scope,granted_at) VALUES('agents-dashboard-staging','profile',unixepoch());
INSERT INTO oauth_client_scopes(client_id,scope,granted_at) VALUES('agents-dashboard-staging','offline_access',unixepoch());
INSERT INTO oauth_client_keys(client_id,kid,algorithm,public_jwk_json,created_at) VALUES('agents-dashboard-staging','agents-dashboard-staging-2026-10','ES256','{"kty":"EC","x":"QxppSGNYvsrRgUZkcfiqO88-zLM_XXN972c1x4QvcUw","y":"STc9XIinVHs_YbCcaw9QqHL3rUUUcHgCUg04ON1YHGA","crv":"P-256","kid":"agents-dashboard-staging-2026-10","alg":"ES256","use":"sig"}',unixepoch());
INSERT INTO security_audit_events(audit_event_id,event_name,occurred_at,observed_at,client_id,outcome,correlation_id,policy_revision,context_json) VALUES('01a1691d-24c4-7c60-916f-ba3083a7b81c','identity.oauth_client.created',unixepoch(),unixepoch(),'agents-dashboard-staging','success','01a1691d-24c4-7c60-916f-ba3083a7b81c',1,json_object('client_id','agents-dashboard-staging'));
INSERT INTO audit_archive_outbox(audit_event_id,r2_object_key,next_attempt_at) VALUES('01a1691d-24c4-7c60-916f-ba3083a7b81c',printf('security-audit/unix-day-%d/%s.json',CAST(unixepoch()/86400 AS INTEGER),'01a1691d-24c4-7c60-916f-ba3083a7b81c'),unixepoch());
