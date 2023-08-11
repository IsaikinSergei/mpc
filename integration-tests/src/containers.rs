#![allow(clippy::too_many_arguments)]

use aes_gcm::{Aes256Gcm, KeyInit};
use anyhow::{anyhow, Ok};
use bollard::{container::LogsOptions, network::CreateNetworkOptions, service::Ipam, Docker};
use ed25519_dalek::ed25519::signature::digest::{consts::U32, generic_array::GenericArray};
use ed25519_dalek::{PublicKey as PublicKeyEd25519, Verifier};
use futures::{lock::Mutex, StreamExt};
use hyper::StatusCode;
use mpc_recovery::sign_node::oidc::OidcToken;
use mpc_recovery::{
    msg::{
        AcceptNodePublicKeysRequest, ClaimOidcRequest, ClaimOidcResponse, MpcPkRequest,
        MpcPkResponse, NewAccountRequest, NewAccountResponse, SignRequest, SignResponse,
        UserCredentialsRequest, UserCredentialsResponse,
    },
    relayer::NearRpcAndRelayerClient,
    transaction::{CreateAccountOptions, LimitedAccessKey},
    utils::{
        claim_oidc_request_digest, claim_oidc_response_digest, sign_digest, sign_request_digest,
        user_credentials_request_digest,
    },
};
use multi_party_eddsa::protocols::ExpandedKeyPair;
use near_crypto::{PublicKey, SecretKey};
use near_primitives::transaction::DeleteKeyAction;
use near_primitives::types::{BlockHeight, Nonce};
use near_primitives::{
    account::{AccessKey, AccessKeyPermission},
    delegate_action::{DelegateAction, SignedDelegateAction},
    transaction::{Action, AddKeyAction},
    views::FinalExecutionStatus,
};
use once_cell::sync::Lazy;
use testcontainers::{
    clients::Cli,
    core::{ExecCommand, WaitFor},
    images::generic::GenericImage,
    Container, Image, RunnableImage,
};
use tokio::io::AsyncWriteExt;
use tracing;
use workspaces::AccountId;

use crate::util;

static NETWORK_MUTEX: Lazy<Mutex<i32>> = Lazy::new(|| Mutex::new(0));

pub struct DockerClient {
    pub docker: Docker,
    pub cli: Cli,
}

impl DockerClient {
    pub async fn get_network_ip_address<I: Image>(
        &self,
        container: &Container<'_, I>,
        network: &str,
    ) -> anyhow::Result<String> {
        let network_settings = self
            .docker
            .inspect_container(container.id(), None)
            .await?
            .network_settings
            .ok_or_else(|| anyhow!("missing NetworkSettings on container '{}'", container.id()))?;
        let ip_address = network_settings
            .networks
            .ok_or_else(|| {
                anyhow!(
                    "missing NetworkSettings.Networks on container '{}'",
                    container.id()
                )
            })?
            .get(network)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "container '{}' is not a part of network '{}'",
                    container.id(),
                    network
                )
            })?
            .ip_address
            .ok_or_else(|| {
                anyhow!(
                    "container '{}' belongs to network '{}', but is not assigned an IP address",
                    container.id(),
                    network
                )
            })?;

        Ok(ip_address)
    }

    pub async fn create_network(&self, network: &str) -> anyhow::Result<()> {
        let _lock = &NETWORK_MUTEX.lock().await;
        let list = self.docker.list_networks::<&str>(None).await?;
        if list.iter().any(|n| n.name == Some(network.to_string())) {
            return Ok(());
        }

        let create_network_options = CreateNetworkOptions {
            name: network,
            check_duplicate: true,
            driver: if cfg!(windows) {
                "transparent"
            } else {
                "bridge"
            },
            ipam: Ipam {
                config: None,
                ..Default::default()
            },
            ..Default::default()
        };
        let _response = &self.docker.create_network(create_network_options).await?;

        Ok(())
    }

    pub async fn continuously_print_logs(&self, id: &str) -> anyhow::Result<()> {
        let mut output = self.docker.logs::<String>(
            id,
            Some(LogsOptions {
                follow: true,
                stdout: true,
                stderr: true,
                ..Default::default()
            }),
        );

        // Asynchronous process that pipes docker attach output into stdout.
        // Will die automatically once Docker container output is closed.
        tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();

            while let Some(Result::Ok(output)) = output.next().await {
                stdout
                    .write_all(output.into_bytes().as_ref())
                    .await
                    .unwrap();
                stdout.flush().await.unwrap();
            }
        });

        Ok(())
    }
}

impl Default for DockerClient {
    fn default() -> Self {
        Self {
            docker: Docker::connect_with_local(
                "unix:///var/run/docker.sock",
                // 10 minutes timeout for all requests in case a lot of tests are being ran in parallel.
                600,
                bollard::API_DEFAULT_VERSION,
            )
            .unwrap(),
            cli: Default::default(),
        }
    }
}

pub struct Redis<'a> {
    pub container: Container<'a, GenericImage>,
    pub address: String,
}

impl<'a> Redis<'a> {
    pub async fn run(docker_client: &'a DockerClient, network: &str) -> anyhow::Result<Redis<'a>> {
        tracing::info!("Running Redis container...");
        let image = GenericImage::new("redis", "latest")
            .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"));
        let image: RunnableImage<GenericImage> = image.into();
        let image = image.with_network(network);
        let container = docker_client.cli.run(image);
        let address = docker_client
            .get_network_ip_address(&container, network)
            .await?;

        tracing::info!("Redis container is running at {}", address);
        Ok(Redis { container, address })
    }
}

pub struct Sandbox<'a> {
    pub container: Container<'a, GenericImage>,
    pub address: String,
    pub local_address: String,
}

impl<'a> Sandbox<'a> {
    pub const CONTAINER_RPC_PORT: u16 = 3000;
    pub const CONTAINER_NETWORK_PORT: u16 = 3001;

    pub async fn run(
        docker_client: &'a DockerClient,
        network: &str,
    ) -> anyhow::Result<Sandbox<'a>> {
        tracing::info!("Running sandbox container...");
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        let image = GenericImage::new("ghcr.io/near/sandbox", "latest-aarch64")
            .with_wait_for(WaitFor::Nothing)
            .with_exposed_port(Self::CONTAINER_RPC_PORT);
        #[cfg(target_arch = "x86_64")]
        let image = GenericImage::new("ghcr.io/near/sandbox", "latest")
            .with_wait_for(WaitFor::Nothing)
            .with_exposed_port(Self::CONTAINER_RPC_PORT);
        let image: RunnableImage<GenericImage> = (
            image,
            vec![
                "--rpc-addr".to_string(),
                format!("0.0.0.0:{}", Self::CONTAINER_RPC_PORT),
                "--network-addr".to_string(),
                format!("0.0.0.0:{}", Self::CONTAINER_NETWORK_PORT),
            ],
        )
            .into();
        let image = image.with_network(network);
        let container = docker_client.cli.run(image);
        let address = docker_client
            .get_network_ip_address(&container, network)
            .await?;
        let host_port = container.get_host_port_ipv4(Self::CONTAINER_RPC_PORT);

        container.exec(ExecCommand {
            cmd: format!(
                "bash -c 'while [[ \"$(curl -H \"Content-type: application/json\" -X POST -s -o /dev/null -w ''%{{http_code}}'' -d ''{{
                \"jsonrpc\": \"2.0\",
                \"id\": \"dontcare\",
                \"method\": \"status\",
                \"params\": []
              }}'' localhost:{})\" != \"200\" ]]; do sleep 1; done; echo \"sandbox is ready to accept connections\"'",
                Self::CONTAINER_RPC_PORT
            ),
            ready_conditions: vec![WaitFor::StdErrMessage { message: "ready".to_string() }]
        });

        let full_address = format!("http://{}:{}", address, Self::CONTAINER_RPC_PORT);
        tracing::info!("Sandbox container is running at {}", full_address);
        Ok(Sandbox {
            container,
            address: full_address,
            local_address: format!("http://localhost:{host_port}"),
        })
    }
}

pub struct Relayer<'a> {
    pub container: Container<'a, GenericImage>,
    pub address: String,
    pub local_address: String,
}

impl<'a> Relayer<'a> {
    pub const CONTAINER_PORT: u16 = 3000;

    pub async fn run(
        docker_client: &'a DockerClient,
        network: &str,
        near_rpc: &str,
        redis_hostname: &str,
        relayer_account_id: &AccountId,
        relayer_account_sk: &workspaces::types::SecretKey,
        creator_account_id: &AccountId,
        social_db_id: &AccountId,
        social_account_id: &AccountId,
        social_account_sk: &workspaces::types::SecretKey,
    ) -> anyhow::Result<Relayer<'a>> {
        tracing::info!("Running relayer container...");
        let image = GenericImage::new("ghcr.io/near/pagoda-relayer-rs-fastauth", "latest")
            .with_wait_for(WaitFor::message_on_stdout("listening on"))
            .with_exposed_port(Self::CONTAINER_PORT)
            .with_env_var("RUST_LOG", "DEBUG")
            .with_env_var("NETWORK", "custom")
            .with_env_var("SERVER_PORT", Self::CONTAINER_PORT.to_string())
            .with_env_var("RELAYER_RPC_URL", near_rpc)
            .with_env_var("RELAYER_ACCOUNT_ID", relayer_account_id.to_string())
            .with_env_var("REDIS_HOST", redis_hostname)
            .with_env_var("PUBLIC_KEY", relayer_account_sk.public_key().to_string())
            .with_env_var("PRIVATE_KEY", relayer_account_sk.to_string())
            .with_env_var(
                "RELAYER_WHITELISTED_CONTRACT",
                creator_account_id.to_string(),
            )
            .with_env_var("CUSTOM_SOCIAL_DB_ID", social_db_id.to_string())
            .with_env_var("STORAGE_ACCOUNT_ID", social_account_id.to_string())
            .with_env_var(
                "STORAGE_PUBLIC_KEY",
                social_account_sk.public_key().to_string(),
            )
            .with_env_var("STORAGE_PRIVATE_KEY", social_account_sk.to_string());
        let image: RunnableImage<GenericImage> = image.into();
        let image = image.with_network(network);
        let container = docker_client.cli.run(image);
        let ip_address = docker_client
            .get_network_ip_address(&container, network)
            .await?;
        let host_port = container.get_host_port_ipv4(Self::CONTAINER_PORT);

        let full_address = format!("http://{}:{}", ip_address, Self::CONTAINER_PORT);
        tracing::info!("Relayer container is running at {}", full_address);
        Ok(Relayer {
            container,
            address: full_address,
            local_address: format!("http://localhost:{host_port}"),
        })
    }
}

pub struct Datastore<'a> {
    pub container: Container<'a, GenericImage>,
    pub address: String,
    pub local_address: String,
}

impl<'a> Datastore<'a> {
    pub const CONTAINER_PORT: u16 = 3000;

    pub async fn run(
        docker_client: &'a DockerClient,
        network: &str,
        project_id: &str,
    ) -> anyhow::Result<Datastore<'a>> {
        tracing::info!("Running datastore container...");
        let image = GenericImage::new(
            "gcr.io/google.com/cloudsdktool/google-cloud-cli",
            "436.0.0-emulators",
        )
        .with_wait_for(WaitFor::message_on_stderr("Dev App Server is now running."))
        .with_exposed_port(Self::CONTAINER_PORT)
        .with_entrypoint("gcloud")
        .with_env_var(
            "DATASTORE_EMULATOR_HOST",
            format!("0.0.0.0:{}", Self::CONTAINER_PORT),
        )
        .with_env_var("DATASTORE_PROJECT_ID", project_id);
        let image: RunnableImage<GenericImage> = (
            image,
            vec![
                "beta".to_string(),
                "emulators".to_string(),
                "datastore".to_string(),
                "start".to_string(),
                format!("--project={project_id}"),
                "--host-port".to_string(),
                format!("0.0.0.0:{}", Self::CONTAINER_PORT),
                "--no-store-on-disk".to_string(),
            ],
        )
            .into();
        let image = image.with_network(network);
        let container = docker_client.cli.run(image);
        let ip_address = docker_client
            .get_network_ip_address(&container, network)
            .await?;
        let host_port = container.get_host_port_ipv4(Self::CONTAINER_PORT);

        let full_address = format!("http://{}:{}/", ip_address, Self::CONTAINER_PORT);
        let local_address = format!("http://localhost:{}/", host_port);
        tracing::info!("Datastore container is running at {}", full_address);
        Ok(Datastore {
            container,
            local_address,
            address: full_address,
        })
    }
}

pub struct SignerNode<'a> {
    pub container: Container<'a, GenericImage>,
    pub address: String,
    pub local_address: String,
    node_id: usize,
    sk_share: ExpandedKeyPair,
    cipher_key: GenericArray<u8, U32>,
    gcp_project_id: String,
    gcp_datastore_local_url: String,
}

pub struct SignerNodeApi {
    pub address: String,
    pub node_id: usize,
    pub sk_share: ExpandedKeyPair,
    pub cipher_key: GenericArray<u8, U32>,
    pub gcp_project_id: String,
    pub gcp_datastore_local_url: String,
}

impl<'a> SignerNode<'a> {
    // Container port used for the docker network, does not have to be unique
    const CONTAINER_PORT: u16 = 3000;

    pub async fn run(
        docker_client: &'a DockerClient,
        network: &str,
        node_id: u64,
        sk_share: &ExpandedKeyPair,
        cipher_key: &GenericArray<u8, U32>,
        datastore_url: &str,
        datastore_local_url: &str,
        gcp_project_id: &str,
        firebase_audience_id: &str,
    ) -> anyhow::Result<SignerNode<'a>> {
        tracing::info!("Running signer node container {}...", node_id);
        let image: GenericImage = GenericImage::new("near/mpc-recovery", "latest")
            .with_wait_for(WaitFor::Nothing)
            .with_exposed_port(Self::CONTAINER_PORT)
            .with_env_var("RUST_LOG", "mpc_recovery=DEBUG");
        let image: RunnableImage<GenericImage> = (
            image,
            vec![
                "start-sign".to_string(),
                "--node-id".to_string(),
                node_id.to_string(),
                "--sk-share".to_string(),
                serde_json::to_string(&sk_share)?,
                "--cipher-key".to_string(),
                hex::encode(cipher_key),
                "--web-port".to_string(),
                Self::CONTAINER_PORT.to_string(),
                "--pagoda-firebase-audience-id".to_string(),
                firebase_audience_id.to_string(),
                "--gcp-project-id".to_string(),
                gcp_project_id.to_string(),
                "--gcp-datastore-url".to_string(),
                datastore_url.to_string(),
                "--test".to_string(),
            ],
        )
            .into();
        let image = image.with_network(network);
        let container = docker_client.cli.run(image);
        let ip_address = docker_client
            .get_network_ip_address(&container, network)
            .await?;
        let host_port = container.get_host_port_ipv4(Self::CONTAINER_PORT);

        container.exec(ExecCommand {
            cmd: format!("bash -c 'while [[ \"$(curl -s -o /dev/null -w ''%{{http_code}}'' localhost:{})\" != \"200\" ]]; do sleep 1; done'", Self::CONTAINER_PORT),
            ready_conditions: vec![WaitFor::message_on_stdout("node is ready to accept connections")]
        });

        let full_address = format!("http://{ip_address}:{}", Self::CONTAINER_PORT);
        tracing::info!(
            "Signer node container {} is running at {}",
            node_id,
            full_address
        );
        Ok(SignerNode {
            container,
            address: full_address,
            local_address: format!("http://localhost:{host_port}"),
            node_id: node_id as usize,
            sk_share: sk_share.clone(),
            cipher_key: *cipher_key,
            gcp_project_id: gcp_project_id.to_string(),
            gcp_datastore_local_url: datastore_local_url.to_string(),
        })
    }

    pub fn api(&self) -> SignerNodeApi {
        SignerNodeApi {
            address: self.local_address.clone(),
            node_id: self.node_id,
            sk_share: self.sk_share.clone(),
            cipher_key: self.cipher_key,
            gcp_project_id: self.gcp_project_id.clone(),
            gcp_datastore_local_url: self.gcp_datastore_local_url.clone(),
        }
    }
}

impl SignerNodeApi {
    pub async fn accept_pk_set(
        &self,
        request: AcceptNodePublicKeysRequest,
    ) -> anyhow::Result<(StatusCode, Result<String, String>)> {
        util::post(format!("{}/accept_pk_set", self.address), request).await
    }

    pub async fn run_rotate_node_key(
        &self,
        new_cipher_key: &GenericArray<u8, U32>,
    ) -> anyhow::Result<(Aes256Gcm, Aes256Gcm)> {
        let env = "dev".to_string();
        let gcp_service = mpc_recovery::gcp::GcpService::new(
            env,
            self.gcp_project_id.clone(),
            Some(self.gcp_datastore_local_url.clone()),
        )
        .await?;

        let new_cipher = Aes256Gcm::new(new_cipher_key);
        let old_cipher = Aes256Gcm::new(&self.cipher_key);

        // Do inplace rotation of node key
        mpc_recovery::sign_node::migration::rotate_cipher(
            self.node_id,
            &old_cipher,
            &new_cipher,
            &gcp_service,
            &gcp_service,
        )
        .await?;

        Ok((old_cipher.clone(), new_cipher))
    }
}

pub struct LeaderNode<'a> {
    pub container: Container<'a, GenericImage>,
    pub address: String,
    local_address: String,
}

pub struct LeaderNodeApi {
    pub address: String,
    client: NearRpcAndRelayerClient,
}

impl<'a> LeaderNode<'a> {
    // Container port used for the docker network, does not have to be unique
    const CONTAINER_PORT: u16 = 3000;

    pub async fn run(
        docker_client: &'a DockerClient,
        network: &str,
        sign_nodes: Vec<String>,
        near_rpc: &str,
        relayer_url: &str,
        datastore_url: &str,
        gcp_project_id: &str,
        near_root_account: &AccountId,
        account_creator_id: &AccountId,
        account_creator_sk: &workspaces::types::SecretKey,
        firebase_audience_id: &str,
    ) -> anyhow::Result<LeaderNode<'a>> {
        tracing::info!("Running leader node container...");

        let image = GenericImage::new("near/mpc-recovery", "latest")
            .with_wait_for(WaitFor::Nothing)
            .with_exposed_port(Self::CONTAINER_PORT)
            .with_env_var("RUST_LOG", "mpc_recovery=DEBUG");
        let mut cmd = vec![
            "start-leader".to_string(),
            "--web-port".to_string(),
            Self::CONTAINER_PORT.to_string(),
            "--near-rpc".to_string(),
            near_rpc.to_string(),
            "--relayer-url".to_string(),
            relayer_url.to_string(),
            "--near-root-account".to_string(),
            near_root_account.to_string(),
            "--account-creator-id".to_string(),
            account_creator_id.to_string(),
            "--account-creator-sk".to_string(),
            account_creator_sk.to_string(),
            "--pagoda-firebase-audience-id".to_string(),
            firebase_audience_id.to_string(),
            "--gcp-project-id".to_string(),
            gcp_project_id.to_string(),
            "--gcp-datastore-url".to_string(),
            datastore_url.to_string(),
            "--test".to_string(),
        ];
        for sign_node in sign_nodes {
            cmd.push("--sign-nodes".to_string());
            cmd.push(sign_node);
        }
        let image: RunnableImage<GenericImage> = (image, cmd).into();
        let image = image.with_network(network);
        let container = docker_client.cli.run(image);
        let ip_address = docker_client
            .get_network_ip_address(&container, network)
            .await?;
        let host_port = container.get_host_port_ipv4(Self::CONTAINER_PORT);

        container.exec(ExecCommand {
            cmd: format!("bash -c 'while [[ \"$(curl -s -o /dev/null -w ''%{{http_code}}'' localhost:{})\" != \"200\" ]]; do sleep 1; done'", Self::CONTAINER_PORT),
            ready_conditions: vec![WaitFor::message_on_stdout("node is ready to accept connections")]
        });

        let full_address = format!("http://{ip_address}:{}", Self::CONTAINER_PORT);
        tracing::info!("Leader node container is running at {}", full_address);
        Ok(LeaderNode {
            container,
            address: full_address,
            local_address: format!("http://localhost:{host_port}"),
        })
    }

    pub fn api(&self, near_rpc: &str, relayer_url: &str) -> LeaderNodeApi {
        LeaderNodeApi {
            address: self.local_address.clone(),
            client: NearRpcAndRelayerClient::connect(near_rpc, relayer_url.to_string(), None),
        }
    }
}

impl LeaderNodeApi {
    pub async fn claim_oidc(
        &self,
        request: ClaimOidcRequest,
    ) -> anyhow::Result<(StatusCode, ClaimOidcResponse)> {
        util::post(format!("{}/claim_oidc", self.address), request).await
    }

    pub async fn get_mpc_pk(
        &self,
        request: MpcPkRequest,
    ) -> anyhow::Result<(StatusCode, MpcPkResponse)> {
        util::post(format!("{}/mpc_public_key", self.address), request).await
    }

    pub async fn user_credentials(
        &self,
        request: UserCredentialsRequest,
    ) -> anyhow::Result<(StatusCode, UserCredentialsResponse)> {
        util::post(format!("{}/user_credentials", self.address), request).await
    }

    pub async fn sign(&self, request: SignRequest) -> anyhow::Result<(StatusCode, SignResponse)> {
        util::post(format!("{}/sign", self.address), request).await
    }

    pub async fn new_account(
        &self,
        request: NewAccountRequest,
    ) -> anyhow::Result<(StatusCode, NewAccountResponse)> {
        util::post(format!("{}/new_account", self.address), request).await
    }

    // TODO: move to utils
    // TODO: I would say this need to replace the regular `new_account` once FRP is enforced
    pub async fn new_account_with_helper(
        &self,
        account_id: &str,
        user_fa_public_key: &PublicKey,
        user_la_public_key: Option<LimitedAccessKey>,
        user_secret_key: &SecretKey,
        oidc_token: &OidcToken,
    ) -> anyhow::Result<(StatusCode, NewAccountResponse)> {
        let user_pk = user_secret_key.public_key();

        let limited_access_keys = user_la_public_key.map(|pk| vec![pk]);

        let create_account_options = CreateAccountOptions {
            full_access_keys: Some(vec![user_fa_public_key.clone()]),
            limited_access_keys,
            contract_bytes: None,
        };

        // By signing this digest we are giving the leader node permission to get user recovery pk
        let user_credentials_request_digest =
            user_credentials_request_digest(oidc_token, &user_pk)?;

        let frp_signature = match user_secret_key.sign(&user_credentials_request_digest) {
            near_crypto::Signature::ED25519(k) => k,
            _ => return Err(anyhow::anyhow!("Wrong signature type")),
        };

        let new_account_request = NewAccountRequest {
            near_account_id: account_id.to_string(),
            create_account_options,
            oidc_token: oidc_token.clone(),
            user_credentials_frp_signature: frp_signature,
            frp_public_key: user_pk.clone(),
        };

        self.new_account(new_account_request).await
    }

    // TODO: add_key should me moved to utils in the future, it is not a part of the API
    pub async fn add_key_with_helper(
        &self,
        account_id: &AccountId,
        oidc_token: &OidcToken,
        public_key: &PublicKey,
        recovery_pk: &PublicKey,
        frp_sk: &SecretKey,
        frp_pk: &PublicKey,
    ) -> anyhow::Result<(StatusCode, SignResponse)> {
        // Prepare SignRequest with add key delegate action
        let (block_height, nonce) = self
            .get_key_info_with_helper(account_id, recovery_pk)
            .await?;

        let add_key_delegate_action = self.get_add_key_delegate_action(
            account_id,
            public_key,
            recovery_pk,
            nonce,
            block_height,
        )?;

        let sign_request_digest: Vec<u8> =
            sign_request_digest(&add_key_delegate_action, oidc_token, frp_pk)?;

        let frp_signature = sign_digest(&sign_request_digest, frp_sk)?;
        let user_credentials_request_digest = user_credentials_request_digest(oidc_token, frp_pk)?;
        let user_credentials_frp_signature = sign_digest(&user_credentials_request_digest, frp_sk)?;

        let sign_request = SignRequest {
            delegate_action: add_key_delegate_action.clone(),
            oidc_token: oidc_token.clone(),
            frp_signature,
            user_credentials_frp_signature,
            frp_public_key: frp_pk.clone(),
        };
        // Send SignRequest to leader node
        let (status_code, sign_response): (_, SignResponse) = self.sign(sign_request).await?;
        let signature = match &sign_response {
            SignResponse::Ok { signature } => signature,
            SignResponse::Err { .. } => return Ok((status_code, sign_response)),
        };
        let response = self
            .client
            .send_meta_tx(SignedDelegateAction {
                delegate_action: add_key_delegate_action,
                signature: near_crypto::Signature::ED25519(*signature),
            })
            .await?;
        if matches!(response.status, FinalExecutionStatus::SuccessValue(_)) {
            Ok((status_code, sign_response))
        } else {
            Err(anyhow::anyhow!("add_key failed with {:?}", response.status))
        }
    }

    pub async fn delete_key_with_helper(
        &self,
        account_id: &AccountId,
        oidc_token: &OidcToken,
        public_key: &PublicKey,
        recovery_pk: &PublicKey,
        frp_sk: &SecretKey,
        frp_pk: &PublicKey,
    ) -> anyhow::Result<(StatusCode, SignResponse)> {
        // Prepare SignRequest with add key delegate action
        let (block_height, nonce) = self
            .get_key_info_with_helper(account_id, recovery_pk)
            .await?;

        let delete_key_delegate_action = self.get_delete_key_delegate_action(
            account_id,
            public_key,
            recovery_pk,
            nonce,
            block_height,
        )?;

        let sign_request_digest =
            sign_request_digest(&delete_key_delegate_action, oidc_token, frp_pk)?;

        let frp_signature = sign_digest(&sign_request_digest, frp_sk)?;
        let user_credentials_request_digest = user_credentials_request_digest(oidc_token, frp_pk)?;
        let user_credentials_frp_signature = sign_digest(&user_credentials_request_digest, frp_sk)?;

        let sign_request = SignRequest {
            delegate_action: delete_key_delegate_action.clone(),
            oidc_token: oidc_token.clone(),
            frp_signature,
            user_credentials_frp_signature,
            frp_public_key: frp_pk.clone(),
        };
        // Send SignRequest to leader node
        let (status_code, sign_response): (_, SignResponse) = self.sign(sign_request).await?;
        let signature = match &sign_response {
            SignResponse::Ok { signature } => signature,
            SignResponse::Err { .. } => return Ok((status_code, sign_response)),
        };
        let response = self
            .client
            .send_meta_tx(SignedDelegateAction {
                delegate_action: delete_key_delegate_action,
                signature: near_crypto::Signature::ED25519(*signature),
            })
            .await?;
        if matches!(response.status, FinalExecutionStatus::SuccessValue(_)) {
            Ok((status_code, sign_response))
        } else {
            Err(anyhow::anyhow!(
                "delete_key failed with {:?}",
                response.status
            ))
        }
    }

    pub async fn perform_delegate_action_with_helper(
        &self,
        delegate_action: &DelegateAction,
        oidc_token: &OidcToken,
        frp_sk: &SecretKey,
        frp_pk: &PublicKey,
    ) -> anyhow::Result<(StatusCode, SignResponse)> {
        let sign_request_digest = sign_request_digest(delegate_action, oidc_token, frp_pk)?;
        let frp_signature = sign_digest(&sign_request_digest, frp_sk)?;

        let user_credentials_request_digest = user_credentials_request_digest(oidc_token, frp_pk)?;
        let user_credentials_frp_signature = sign_digest(&user_credentials_request_digest, frp_sk)?;

        let sign_request = SignRequest {
            delegate_action: delegate_action.clone(),
            oidc_token: oidc_token.clone(),
            frp_signature,
            user_credentials_frp_signature,
            frp_public_key: frp_pk.clone(),
        };
        // Send SignRequest to leader node
        let (status_code, sign_response): (_, SignResponse) = self.sign(sign_request).await?;
        Ok((status_code, sign_response))
    }

    // TODO: move to utils
    pub async fn claim_oidc_with_helper(
        &self,
        oidc_token: &OidcToken,
        user_public_key: &PublicKey,
        user_secret_key: &SecretKey,
    ) -> anyhow::Result<(StatusCode, ClaimOidcResponse)> {
        let oidc_token_hash = oidc_token.digest_hash();

        let request_digest = claim_oidc_request_digest(&oidc_token_hash, user_public_key).unwrap();
        let request_digest_signature = sign_digest(&request_digest, user_secret_key)?;

        let oidc_request = ClaimOidcRequest {
            oidc_token_hash,
            frp_public_key: user_public_key.to_string(),
            frp_signature: request_digest_signature,
        };

        let response = self.claim_oidc(oidc_request.clone()).await?;

        match response.1 {
            ClaimOidcResponse::Ok { mpc_signature } => {
                let mpc_pk: PublicKeyEd25519 =
                    self.get_mpc_pk(MpcPkRequest {}).await?.1.try_into()?;

                // Verify signature
                let response_digest = claim_oidc_response_digest(oidc_request.frp_signature)?;
                mpc_pk.verify(&response_digest, &mpc_signature)?;
                Ok(response)
            }
            ClaimOidcResponse::Err { .. } => Ok(response),
        }
    }

    pub async fn user_credentials_with_helper(
        &self,
        oidc_token: &OidcToken,
        client_sk: &SecretKey,
        client_pk: &PublicKey,
    ) -> anyhow::Result<(StatusCode, UserCredentialsResponse)> {
        let user_credentials_request_digest =
            user_credentials_request_digest(oidc_token, client_pk)?;

        let frp_signature = match client_sk.sign(&user_credentials_request_digest) {
            near_crypto::Signature::ED25519(k) => k,
            _ => return Err(anyhow::anyhow!("Wrong signature type")),
        };

        self.user_credentials(UserCredentialsRequest {
            oidc_token: oidc_token.clone(),
            frp_signature,
            frp_public_key: client_pk.clone(),
        })
        .await
    }

    pub fn get_add_key_delegate_action(
        &self,
        account_id: &AccountId,
        public_key: &PublicKey,
        recovery_pk: &PublicKey,
        nonce: u64,
        block_height: u64,
    ) -> anyhow::Result<DelegateAction> {
        Ok(DelegateAction {
            sender_id: account_id.clone(),
            receiver_id: account_id.clone(),
            actions: vec![Action::AddKey(AddKeyAction {
                public_key: public_key.clone(),
                access_key: AccessKey {
                    nonce: 0,
                    permission: AccessKeyPermission::FullAccess,
                },
            })
            .try_into()?],
            nonce,
            max_block_height: block_height + 100,
            public_key: recovery_pk.clone(),
        })
    }

    pub fn get_delete_key_delegate_action(
        &self,
        account_id: &AccountId,
        public_key: &PublicKey,
        recovery_pk: &PublicKey,
        nonce: u64,
        block_height: u64,
    ) -> anyhow::Result<DelegateAction> {
        Ok(DelegateAction {
            sender_id: account_id.clone(),
            receiver_id: account_id.clone(),
            actions: vec![Action::DeleteKey(DeleteKeyAction {
                public_key: public_key.clone(),
            })
            .try_into()?],
            nonce,
            max_block_height: block_height + 100,
            public_key: recovery_pk.clone(),
        })
    }

    pub async fn get_key_info_with_helper(
        &self,
        account_id: &AccountId,
        pk: &PublicKey,
    ) -> anyhow::Result<(BlockHeight, Nonce)> {
        let (_, block_height, nonce) = self
            .client
            .access_key(account_id.clone(), pk.clone())
            .await?;
        Ok((block_height, nonce))
    }
}
