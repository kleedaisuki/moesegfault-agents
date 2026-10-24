-- Reviewed production registration: both hosts are controlled by the authorized deployment owner.
-- Shared Subscribe billing sector is intentional. Generic generator guard and auth logic remain unchanged.
-- Deployment-owned OAuth client; public metadata only. / 部署所有的 OAuth 客户端，仅含公开元数据。
-- Apply as a reviewed forward-only D1 migration, not as separate CLI statements. / 作为已评审的只向前 D1 migration 应用，不得逐条执行。
INSERT INTO oauth_clients(client_id,display_name,client_type,token_endpoint_auth_method,sector_identifier,subject_salt_revision,state,created_at,updated_at) VALUES('agents-dashboard','moeSegFault Agents dashboard','confidential','private_key_jwt','subscribe.moesegfault.dev',1,'enabled',unixepoch(),unixepoch());
INSERT INTO oauth_redirect_uris(redirect_uri_id,client_id,redirect_uri,match_mode,created_at) VALUES('01a16c29-0af9-721a-bae8-b3c08c5aba9e','agents-dashboard','https://adash.moesegfault.dev/auth/callback','exact',unixepoch());
INSERT INTO oauth_post_logout_redirect_uris(post_logout_redirect_uri_id,client_id,redirect_uri,created_at) VALUES('01a16c29-0af9-7fcc-8306-2b1f08c01a49','agents-dashboard','https://adash.moesegfault.dev/',unixepoch());
INSERT INTO oauth_client_scopes(client_id,scope,granted_at) VALUES('agents-dashboard','openid',unixepoch());
INSERT INTO oauth_client_scopes(client_id,scope,granted_at) VALUES('agents-dashboard','profile',unixepoch());
INSERT INTO oauth_client_scopes(client_id,scope,granted_at) VALUES('agents-dashboard','offline_access',unixepoch());
INSERT INTO oauth_client_keys(client_id,kid,algorithm,public_jwk_json,created_at) VALUES('agents-dashboard','agents-dashboard-production-2026-10','ES256','{"kty":"EC","x":"5SR7DwbyqQaPq_0qMQBAd8fAWMNZ6mxfZHVZYWIMqkE","y":"QpEoXjrHkTexiIP4N0H6Zo7Tc_HzVBoJgLLN6LwCsVo","crv":"P-256","kid":"agents-dashboard-production-2026-10","alg":"ES256","use":"sig"}',unixepoch());
INSERT INTO security_audit_events(audit_event_id,event_name,occurred_at,observed_at,client_id,outcome,correlation_id,policy_revision,context_json) VALUES('01a16c29-0af9-7c27-aaea-4d52044a1777','identity.oauth_client.created',unixepoch(),unixepoch(),'agents-dashboard','success','01a16c29-0af9-7c27-aaea-4d52044a1777',1,json_object('client_id','agents-dashboard'));
INSERT INTO audit_archive_outbox(audit_event_id,r2_object_key,next_attempt_at) VALUES('01a16c29-0af9-7c27-aaea-4d52044a1777',printf('security-audit/unix-day-%d/%s.json',CAST(unixepoch()/86400 AS INTEGER),'01a16c29-0af9-7c27-aaea-4d52044a1777'),unixepoch());
