SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: pgcrypto; Type: EXTENSION; Schema: -; Owner: -
--

CREATE EXTENSION IF NOT EXISTS pgcrypto WITH SCHEMA public;


--
-- Name: EXTENSION pgcrypto; Type: COMMENT; Schema: -; Owner: -
--

COMMENT ON EXTENSION pgcrypto IS 'cryptographic functions';


--
-- Name: update_updated_at(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.update_updated_at() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END;
$$;


--
-- Name: validate_api_key(text); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.validate_api_key(p_key_hash text) RETURNS TABLE(account_id uuid, tier text, active boolean, api_key_id uuid)
    LANGUAGE plpgsql
    AS $$
BEGIN
    RETURN QUERY
    SELECT a.id AS account_id, a.tier, a.active, k.id AS api_key_id
    FROM api_keys k
    JOIN accounts a ON a.id = k.account_id
    WHERE k.key_hash = p_key_hash
      AND k.active = true
      AND a.active = true;

    -- Update last_used_at
    UPDATE api_keys SET last_used_at = now() WHERE api_keys.key_hash = p_key_hash AND api_keys.active = true;
END;
$$;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: account_invites; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_invites (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    account_id uuid NOT NULL,
    email text NOT NULL,
    role text DEFAULT 'member'::text NOT NULL,
    invited_by uuid NOT NULL,
    token_hash text NOT NULL,
    status text DEFAULT 'pending'::text NOT NULL,
    expires_at timestamp with time zone DEFAULT (now() + '7 days'::interval) NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT account_invites_role_check CHECK ((role = ANY (ARRAY['admin'::text, 'member'::text, 'viewer'::text]))),
    CONSTRAINT account_invites_status_check CHECK ((status = ANY (ARRAY['pending'::text, 'accepted'::text, 'expired'::text, 'revoked'::text])))
);


--
-- Name: account_members; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_members (
    user_id uuid NOT NULL,
    account_id uuid NOT NULL,
    role text DEFAULT 'owner'::text NOT NULL,
    joined_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT account_members_role_check CHECK ((role = ANY (ARRAY['owner'::text, 'admin'::text, 'member'::text, 'viewer'::text])))
);


--
-- Name: accounts; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.accounts (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    name text NOT NULL,
    tier text DEFAULT 'free'::text NOT NULL,
    active boolean DEFAULT true NOT NULL,
    stripe_customer_id text,
    stripe_default_payment_method_id text,
    credits_balance bigint DEFAULT 100 NOT NULL,
    auto_topup_enabled boolean DEFAULT false NOT NULL,
    auto_topup_amount bigint DEFAULT 5000 NOT NULL,
    auto_topup_threshold bigint DEFAULT 500 NOT NULL,
    monthly_spend_limit bigint,
    created_at timestamp(6) without time zone DEFAULT now() NOT NULL,
    updated_at timestamp(6) without time zone DEFAULT now() NOT NULL,
    CONSTRAINT accounts_tier_check CHECK ((tier = ANY (ARRAY['free'::text, 'starter'::text, 'pro'::text, 'enterprise'::text])))
);


--
-- Name: api_keys; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.api_keys (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    account_id uuid NOT NULL,
    name text NOT NULL,
    prefix text NOT NULL,
    key_hash text NOT NULL,
    active boolean DEFAULT true NOT NULL,
    last_used_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: ar_internal_metadata; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.ar_internal_metadata (
    key character varying NOT NULL,
    value character varying,
    created_at timestamp(6) without time zone NOT NULL,
    updated_at timestamp(6) without time zone NOT NULL
);


--
-- Name: crawl_configs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.crawl_configs (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    account_id uuid NOT NULL,
    name text NOT NULL,
    description text,
    config jsonb NOT NULL,
    cron_expression text,
    cron_enabled boolean DEFAULT false NOT NULL,
    last_run_at timestamp with time zone,
    next_run_at timestamp with time zone,
    last_job_id text,
    created_at timestamp(6) without time zone DEFAULT now() NOT NULL,
    updated_at timestamp(6) without time zone DEFAULT now() NOT NULL
);


--
-- Name: jobs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.jobs (
    job_id text NOT NULL,
    status text DEFAULT 'pending'::text NOT NULL,
    index_uid text NOT NULL,
    account_id uuid,
    api_key_id uuid,
    pages_crawled bigint DEFAULT 0 NOT NULL,
    pages_indexed bigint DEFAULT 0 NOT NULL,
    documents_sent bigint DEFAULT 0 NOT NULL,
    errors bigint DEFAULT 0 NOT NULL,
    bytes_downloaded bigint DEFAULT 0 NOT NULL,
    started_at timestamp with time zone,
    completed_at timestamp with time zone,
    crawl_rate double precision DEFAULT 0.0 NOT NULL,
    eta_seconds bigint,
    error_message text,
    start_urls jsonb DEFAULT '[]'::jsonb NOT NULL,
    max_pages bigint,
    config jsonb,
    swap_temp_index text,
    swap_meilisearch_url text,
    swap_meilisearch_api_key text,
    created_at timestamp(6) without time zone DEFAULT now() NOT NULL,
    updated_at timestamp(6) without time zone DEFAULT now() NOT NULL,
    CONSTRAINT jobs_status_check CHECK ((status = ANY (ARRAY['pending'::text, 'running'::text, 'completed'::text, 'failed'::text, 'cancelled'::text, 'paused'::text])))
);


--
-- Name: meilisearch_engines; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.meilisearch_engines (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    account_id uuid NOT NULL,
    name text NOT NULL,
    url text NOT NULL,
    api_key text DEFAULT ''::text NOT NULL,
    is_default boolean DEFAULT false NOT NULL,
    created_at timestamp(6) without time zone DEFAULT now() NOT NULL,
    updated_at timestamp(6) without time zone DEFAULT now() NOT NULL
);


--
-- Name: oauth_authorization_codes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.oauth_authorization_codes (
    code character varying(128) NOT NULL,
    client_id character varying(64) NOT NULL,
    user_id uuid NOT NULL,
    redirect_uri text NOT NULL,
    scope character varying(255) DEFAULT 'mcp'::character varying,
    code_challenge character varying(128) NOT NULL,
    code_challenge_method character varying(10) DEFAULT 'S256'::character varying NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    used boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: oauth_clients; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.oauth_clients (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    client_id character varying(64) NOT NULL,
    client_name character varying(255),
    redirect_uris text[] NOT NULL,
    scope character varying(255) DEFAULT 'mcp'::character varying,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: oauth_identities; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.oauth_identities (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    user_id uuid NOT NULL,
    provider text NOT NULL,
    provider_user_id text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT oauth_identities_provider_check CHECK ((provider = ANY (ARRAY['google'::text, 'github'::text])))
);


--
-- Name: oauth_tokens; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.oauth_tokens (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    token_hash character varying(64) NOT NULL,
    token_type character varying(16) NOT NULL,
    client_id character varying(64) NOT NULL,
    user_id uuid NOT NULL,
    scope character varying(255) DEFAULT 'mcp'::character varying,
    expires_at timestamp with time zone NOT NULL,
    revoked boolean DEFAULT false NOT NULL,
    parent_token_id uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT oauth_tokens_token_type_check CHECK (((token_type)::text = ANY ((ARRAY['access'::character varying, 'refresh'::character varying])::text[])))
);


--
-- Name: password_reset_tokens; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.password_reset_tokens (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    user_id uuid NOT NULL,
    token_hash text NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    used boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: scheduled_emails; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.scheduled_emails (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    email_type text NOT NULL,
    recipient text NOT NULL,
    payload jsonb DEFAULT '{}'::jsonb NOT NULL,
    send_at timestamp with time zone NOT NULL,
    sent boolean DEFAULT false NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    next_attempt_at timestamp with time zone,
    last_error text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: schema_migrations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.schema_migrations (
    version character varying NOT NULL
);


--
-- Name: transactions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.transactions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    account_id uuid NOT NULL,
    type text NOT NULL,
    amount bigint NOT NULL,
    balance_after bigint NOT NULL,
    description text,
    metadata jsonb,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT transactions_type_check CHECK ((type = ANY (ARRAY['initial_deposit'::text, 'manual_topup'::text, 'auto_topup'::text, 'usage_deduction'::text, 'refund'::text, 'adjustment'::text])))
);


--
-- Name: users; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.users (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    email text NOT NULL,
    password_hash text,
    full_name text,
    email_verified boolean DEFAULT false NOT NULL,
    email_verification_token text,
    notify_job_emails boolean DEFAULT true NOT NULL,
    created_at timestamp(6) without time zone DEFAULT now() NOT NULL,
    updated_at timestamp(6) without time zone DEFAULT now() NOT NULL
);


--
-- Name: account_invites account_invites_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_invites
    ADD CONSTRAINT account_invites_pkey PRIMARY KEY (id);


--
-- Name: account_members account_members_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_members
    ADD CONSTRAINT account_members_pkey PRIMARY KEY (user_id, account_id);


--
-- Name: accounts accounts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.accounts
    ADD CONSTRAINT accounts_pkey PRIMARY KEY (id);


--
-- Name: api_keys api_keys_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.api_keys
    ADD CONSTRAINT api_keys_pkey PRIMARY KEY (id);


--
-- Name: ar_internal_metadata ar_internal_metadata_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ar_internal_metadata
    ADD CONSTRAINT ar_internal_metadata_pkey PRIMARY KEY (key);


--
-- Name: crawl_configs crawl_configs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.crawl_configs
    ADD CONSTRAINT crawl_configs_pkey PRIMARY KEY (id);


--
-- Name: jobs jobs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.jobs
    ADD CONSTRAINT jobs_pkey PRIMARY KEY (job_id);


--
-- Name: meilisearch_engines meilisearch_engines_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meilisearch_engines
    ADD CONSTRAINT meilisearch_engines_pkey PRIMARY KEY (id);


--
-- Name: oauth_authorization_codes oauth_authorization_codes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_authorization_codes
    ADD CONSTRAINT oauth_authorization_codes_pkey PRIMARY KEY (code);


--
-- Name: oauth_clients oauth_clients_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_clients
    ADD CONSTRAINT oauth_clients_pkey PRIMARY KEY (id);


--
-- Name: oauth_identities oauth_identities_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_identities
    ADD CONSTRAINT oauth_identities_pkey PRIMARY KEY (id);


--
-- Name: oauth_tokens oauth_tokens_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_tokens
    ADD CONSTRAINT oauth_tokens_pkey PRIMARY KEY (id);


--
-- Name: password_reset_tokens password_reset_tokens_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.password_reset_tokens
    ADD CONSTRAINT password_reset_tokens_pkey PRIMARY KEY (id);


--
-- Name: scheduled_emails scheduled_emails_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.scheduled_emails
    ADD CONSTRAINT scheduled_emails_pkey PRIMARY KEY (id);


--
-- Name: schema_migrations schema_migrations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.schema_migrations
    ADD CONSTRAINT schema_migrations_pkey PRIMARY KEY (version);


--
-- Name: transactions transactions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.transactions
    ADD CONSTRAINT transactions_pkey PRIMARY KEY (id);


--
-- Name: users users_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);


--
-- Name: index_account_invites_on_account_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_account_invites_on_account_id ON public.account_invites USING btree (account_id);


--
-- Name: index_account_invites_on_account_id_and_email_pending; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX index_account_invites_on_account_id_and_email_pending ON public.account_invites USING btree (account_id, email) WHERE (status = 'pending'::text);


--
-- Name: index_account_invites_on_email; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_account_invites_on_email ON public.account_invites USING btree (email);


--
-- Name: index_account_invites_on_token_hash_pending; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_account_invites_on_token_hash_pending ON public.account_invites USING btree (token_hash) WHERE (status = 'pending'::text);


--
-- Name: index_account_members_on_account_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_account_members_on_account_id ON public.account_members USING btree (account_id);


--
-- Name: index_account_members_on_user_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_account_members_on_user_id ON public.account_members USING btree (user_id);


--
-- Name: index_api_keys_on_account_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_api_keys_on_account_id ON public.api_keys USING btree (account_id);


--
-- Name: index_api_keys_on_key_hash; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_api_keys_on_key_hash ON public.api_keys USING btree (key_hash);


--
-- Name: index_crawl_configs_on_account_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_crawl_configs_on_account_id ON public.crawl_configs USING btree (account_id);


--
-- Name: index_crawl_configs_on_account_id_and_name; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX index_crawl_configs_on_account_id_and_name ON public.crawl_configs USING btree (account_id, name);


--
-- Name: index_crawl_configs_on_next_run_at_due; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_crawl_configs_on_next_run_at_due ON public.crawl_configs USING btree (next_run_at) WHERE ((cron_enabled = true) AND (cron_expression IS NOT NULL));


--
-- Name: index_jobs_on_account_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_jobs_on_account_id ON public.jobs USING btree (account_id);


--
-- Name: index_jobs_on_created_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_jobs_on_created_at ON public.jobs USING btree (created_at DESC);


--
-- Name: index_jobs_on_job_id_active; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_jobs_on_job_id_active ON public.jobs USING btree (job_id) WHERE (status = ANY (ARRAY['pending'::text, 'running'::text, 'paused'::text]));


--
-- Name: index_jobs_on_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_jobs_on_status ON public.jobs USING btree (status);


--
-- Name: index_meilisearch_engines_on_account_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_meilisearch_engines_on_account_id ON public.meilisearch_engines USING btree (account_id);


--
-- Name: index_meilisearch_engines_on_account_id_and_name; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX index_meilisearch_engines_on_account_id_and_name ON public.meilisearch_engines USING btree (account_id, name);


--
-- Name: index_oauth_authorization_codes_on_expires_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_oauth_authorization_codes_on_expires_at ON public.oauth_authorization_codes USING btree (expires_at);


--
-- Name: index_oauth_clients_on_client_id; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX index_oauth_clients_on_client_id ON public.oauth_clients USING btree (client_id);


--
-- Name: index_oauth_identities_on_provider_and_provider_user_id; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX index_oauth_identities_on_provider_and_provider_user_id ON public.oauth_identities USING btree (provider, provider_user_id);


--
-- Name: index_oauth_identities_on_user_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_oauth_identities_on_user_id ON public.oauth_identities USING btree (user_id);


--
-- Name: index_oauth_tokens_on_token_hash; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX index_oauth_tokens_on_token_hash ON public.oauth_tokens USING btree (token_hash);


--
-- Name: index_oauth_tokens_on_token_hash_live; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_oauth_tokens_on_token_hash_live ON public.oauth_tokens USING btree (token_hash) WHERE (revoked = false);


--
-- Name: index_password_reset_tokens_on_token_hash_usable; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_password_reset_tokens_on_token_hash_usable ON public.password_reset_tokens USING btree (token_hash) WHERE (used = false);


--
-- Name: index_password_reset_tokens_on_user_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_password_reset_tokens_on_user_id ON public.password_reset_tokens USING btree (user_id);


--
-- Name: index_scheduled_emails_on_send_at_pending; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_scheduled_emails_on_send_at_pending ON public.scheduled_emails USING btree (send_at) WHERE (sent = false);


--
-- Name: index_transactions_on_account_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_transactions_on_account_id ON public.transactions USING btree (account_id);


--
-- Name: index_transactions_on_account_id_and_created_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_transactions_on_account_id_and_created_at ON public.transactions USING btree (account_id, created_at DESC);


--
-- Name: index_transactions_on_created_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_transactions_on_created_at ON public.transactions USING btree (created_at DESC);


--
-- Name: index_users_on_email; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX index_users_on_email ON public.users USING btree (email);


--
-- Name: jobs trg_jobs_updated_at; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER trg_jobs_updated_at BEFORE UPDATE ON public.jobs FOR EACH ROW EXECUTE FUNCTION public.update_updated_at();


--
-- Name: transactions fk_rails_01f020e267; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.transactions
    ADD CONSTRAINT fk_rails_01f020e267 FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: password_reset_tokens fk_rails_1dfd31e72f; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.password_reset_tokens
    ADD CONSTRAINT fk_rails_1dfd31e72f FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: oauth_authorization_codes fk_rails_234c3254d2; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_authorization_codes
    ADD CONSTRAINT fk_rails_234c3254d2 FOREIGN KEY (client_id) REFERENCES public.oauth_clients(client_id);


--
-- Name: oauth_authorization_codes fk_rails_2df019e87a; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_authorization_codes
    ADD CONSTRAINT fk_rails_2df019e87a FOREIGN KEY (user_id) REFERENCES public.users(id);


--
-- Name: oauth_identities fk_rails_2f75762ff1; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_identities
    ADD CONSTRAINT fk_rails_2f75762ff1 FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: oauth_tokens fk_rails_33f93a408a; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_tokens
    ADD CONSTRAINT fk_rails_33f93a408a FOREIGN KEY (parent_token_id) REFERENCES public.oauth_tokens(id);


--
-- Name: account_members fk_rails_691c5572ed; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_members
    ADD CONSTRAINT fk_rails_691c5572ed FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_invites fk_rails_7f726cca01; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_invites
    ADD CONSTRAINT fk_rails_7f726cca01 FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_members fk_rails_8ac6681de2; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_members
    ADD CONSTRAINT fk_rails_8ac6681de2 FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: jobs fk_rails_95b364200b; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.jobs
    ADD CONSTRAINT fk_rails_95b364200b FOREIGN KEY (api_key_id) REFERENCES public.api_keys(id) ON DELETE SET NULL;


--
-- Name: account_invites fk_rails_993670d6d0; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_invites
    ADD CONSTRAINT fk_rails_993670d6d0 FOREIGN KEY (invited_by) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: oauth_tokens fk_rails_b395e0595f; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_tokens
    ADD CONSTRAINT fk_rails_b395e0595f FOREIGN KEY (client_id) REFERENCES public.oauth_clients(client_id);


--
-- Name: jobs fk_rails_c31d0a1ae2; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.jobs
    ADD CONSTRAINT fk_rails_c31d0a1ae2 FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE SET NULL;


--
-- Name: oauth_tokens fk_rails_dd28cc2ffe; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_tokens
    ADD CONSTRAINT fk_rails_dd28cc2ffe FOREIGN KEY (user_id) REFERENCES public.users(id);


--
-- Name: crawl_configs fk_rails_eda3dae7b4; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.crawl_configs
    ADD CONSTRAINT fk_rails_eda3dae7b4 FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: api_keys fk_rails_f4470e16d5; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.api_keys
    ADD CONSTRAINT fk_rails_f4470e16d5 FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: meilisearch_engines fk_rails_f9e61d80b5; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.meilisearch_engines
    ADD CONSTRAINT fk_rails_f9e61d80b5 FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- PostgreSQL database dump complete
--

SET search_path TO "$user", public;

INSERT INTO "schema_migrations" (version) VALUES
('20260817000014'),
('20260817000013'),
('20260817000012'),
('20260817000011'),
('20260817000010'),
('20260817000009'),
('20260817000008'),
('20260817000007'),
('20260817000006'),
('20260817000005'),
('20260817000004'),
('20260817000003'),
('20260817000002'),
('20260817000001');

