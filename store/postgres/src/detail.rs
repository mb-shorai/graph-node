//! Queries to support the index node API
use anyhow::anyhow;
use diesel::pg::PgConnection;
use diesel::prelude::{
    ExpressionMethods, JoinOnDsl, NullableExpressionMethods, QueryDsl, RunQueryDsl,
};
use diesel_derives::Associations;
use git2::{self, Commit, ObjectType, Repository, StatusOptions};
use graph::{
    constraint_violation,
    data::subgraph::schema::{SubgraphError, SubgraphManifestEntity},
    prelude::{
        bigdecimal::ToPrimitive, BigDecimal, BlockPtr, DeploymentHash, StoreError,
        SubgraphDeploymentEntity,
    },
};
use graph::{data::subgraph::status, prelude::web3::types::H256};
use std::convert::TryFrom;
use std::{ops::Bound, sync::Arc};

use crate::primary::Site;
use crate::{
    deployment::{
        graph_node_versions, subgraph_deployment, subgraph_error, subgraph_manifest,
        SubgraphHealth as HealthType,
    },
    primary::DeploymentId,
};

const CARGO_MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");
const CARGO_PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const CARGO_PKG_VERSION_MAJOR: &str = env!("CARGO_PKG_VERSION_MAJOR");
const CARGO_PKG_VERSION_MINOR: &str = env!("CARGO_PKG_VERSION_MINOR");
const CARGO_PKG_VERSION_PATCH: &str = env!("CARGO_PKG_VERSION_PATCH");
const CARGO_PKG_VERSION_PRE: &str = env!("CARGO_PKG_VERSION_PRE");

type Bytes = Vec<u8>;

#[derive(Queryable, QueryableByName)]
#[table_name = "subgraph_deployment"]
// We map all fields to make loading `Detail` with diesel easier, but we
// don't need all the fields
#[allow(dead_code)]
pub struct DeploymentDetail {
    pub id: DeploymentId,
    pub deployment: String,
    pub failed: bool,
    health: HealthType,
    pub synced: bool,
    fatal_error: Option<String>,
    non_fatal_errors: Vec<String>,
    earliest_ethereum_block_hash: Option<Bytes>,
    earliest_ethereum_block_number: Option<BigDecimal>,
    pub latest_ethereum_block_hash: Option<Bytes>,
    pub latest_ethereum_block_number: Option<BigDecimal>,
    last_healthy_ethereum_block_hash: Option<Bytes>,
    last_healthy_ethereum_block_number: Option<BigDecimal>,
    pub entity_count: BigDecimal,
    graft_base: Option<String>,
    graft_block_hash: Option<Bytes>,
    graft_block_number: Option<BigDecimal>,
    reorg_count: i32,
    current_reorg_depth: i32,
    max_reorg_depth: i32,
}

#[derive(Queryable, QueryableByName)]
#[table_name = "subgraph_error"]
// We map all fields to make loading `Detail` with diesel easier, but we
// don't need all the fields
#[allow(dead_code)]
struct ErrorDetail {
    vid: i64,
    id: String,
    subgraph_id: String,
    message: String,
    block_hash: Option<Bytes>,
    handler: Option<String>,
    deterministic: bool,
    block_range: (Bound<i32>, Bound<i32>),
}

struct DetailAndError<'a>(DeploymentDetail, Option<ErrorDetail>, &'a Vec<Arc<Site>>);

pub(crate) fn block(
    id: &str,
    name: &str,
    hash: Option<Vec<u8>>,
    number: Option<BigDecimal>,
) -> Result<Option<status::EthereumBlock>, StoreError> {
    match (&hash, &number) {
        (Some(hash), Some(number)) => {
            let hash = H256::from_slice(hash.as_slice());
            let number = number.to_u64().ok_or_else(|| {
                constraint_violation!(
                    "the block number {} for {} in {} is not representable as a u64",
                    number,
                    name,
                    id
                )
            })?;
            Ok(Some(status::EthereumBlock::new(hash, number)))
        }
        (None, None) => Ok(None),
        _ => Err(constraint_violation!(
            "the hash and number \
        of a block pointer must either both be null or both have a \
        value, but for `{}` the hash of {} is `{:?}` and the number is `{:?}`",
            id,
            name,
            hash,
            number
        )),
    }
}

impl TryFrom<ErrorDetail> for SubgraphError {
    type Error = StoreError;

    fn try_from(value: ErrorDetail) -> Result<Self, Self::Error> {
        let ErrorDetail {
            vid: _,
            id: _,
            subgraph_id,
            message,
            block_hash,
            handler,
            deterministic,
            block_range,
        } = value;
        let block_number = crate::block_range::first_block_in_range(&block_range);
        let block_hash = block_hash.map(|hash| H256::from_slice(hash.as_slice()));
        // In existing databases, we have errors that have a `block_range` of
        // `UNVERSIONED_RANGE`, which leads to `None` as the block number, but
        // has a hash. Conversely, it is also possible for an error to not have a
        // hash. In both cases, use a block pointer of `None`
        let block_ptr = match (block_number, block_hash) {
            (Some(number), Some(hash)) => Some(BlockPtr::from((hash, number as u64))),
            _ => None,
        };
        let subgraph_id = DeploymentHash::new(subgraph_id).map_err(|id| {
            StoreError::ConstraintViolation(format!("invalid subgraph id `{}` in fatal error", id))
        })?;
        Ok(SubgraphError {
            subgraph_id,
            message,
            block_ptr,
            handler,
            deterministic,
        })
    }
}

impl<'a> TryFrom<DetailAndError<'a>> for status::Info {
    type Error = StoreError;

    fn try_from(detail_and_error: DetailAndError) -> Result<Self, Self::Error> {
        let DetailAndError(detail, error, sites) = detail_and_error;

        let DeploymentDetail {
            deployment,
            failed: _,
            health,
            synced,
            fatal_error: _,
            non_fatal_errors: _,
            earliest_ethereum_block_hash,
            earliest_ethereum_block_number,
            latest_ethereum_block_hash,
            latest_ethereum_block_number,
            entity_count,
            graft_base: _,
            graft_block_hash: _,
            graft_block_number: _,
            ..
        } = detail;

        let site = sites
            .iter()
            .find(|site| site.deployment.as_str() == &deployment)
            .ok_or_else(|| constraint_violation!("missing site for subgraph `{}`", deployment))?;

        // This needs to be filled in later since it lives in a
        // different shard
        let chain_head_block = None;
        let earliest_block = block(
            &deployment,
            "earliest_ethereum_block",
            earliest_ethereum_block_hash,
            earliest_ethereum_block_number,
        )?;
        let latest_block = block(
            &deployment,
            "latest_ethereum_block",
            latest_ethereum_block_hash,
            latest_ethereum_block_number,
        )?;
        let health = health.into();
        let chain = status::ChainInfo {
            network: site.network.clone(),
            chain_head_block,
            earliest_block,
            latest_block,
        };
        let entity_count = entity_count.to_u64().ok_or_else(|| {
            constraint_violation!(
                "the entityCount for {} is not representable as a u64",
                deployment
            )
        })?;
        let fatal_error = error.map(|e| SubgraphError::try_from(e)).transpose()?;
        // 'node' needs to be filled in later from a different shard
        Ok(status::Info {
            subgraph: deployment,
            synced,
            health,
            fatal_error,
            non_fatal_errors: vec![],
            chains: vec![chain],
            entity_count,
            node: None,
        })
    }
}

/// Return the details for `deployments`
pub(crate) fn deployment_details(
    conn: &PgConnection,
    deployments: Vec<String>,
) -> Result<Vec<DeploymentDetail>, StoreError> {
    use subgraph_deployment as d;

    // Empty deployments means 'all of them'
    let details = if deployments.is_empty() {
        d::table.load::<DeploymentDetail>(conn)?
    } else {
        d::table
            .filter(d::deployment.eq_any(&deployments))
            .load::<DeploymentDetail>(conn)?
    };
    Ok(details)
}

pub(crate) fn deployment_statuses(
    conn: &PgConnection,
    sites: &Vec<Arc<Site>>,
) -> Result<Vec<status::Info>, StoreError> {
    use subgraph_deployment as d;
    use subgraph_error as e;

    // Empty deployments means 'all of them'
    if sites.is_empty() {
        d::table
            .left_outer_join(e::table.on(d::fatal_error.eq(e::id.nullable())))
            .load::<(DeploymentDetail, Option<ErrorDetail>)>(conn)?
            .into_iter()
            .map(|(detail, error)| status::Info::try_from(DetailAndError(detail, error, sites)))
            .collect()
    } else {
        let ids: Vec<_> = sites
            .into_iter()
            .map(|site| site.deployment.to_string())
            .collect();

        d::table
            .left_outer_join(e::table.on(d::fatal_error.eq(e::id.nullable())))
            .filter(d::deployment.eq_any(&ids))
            .load::<(DeploymentDetail, Option<ErrorDetail>)>(conn)?
            .into_iter()
            .map(|(detail, error)| status::Info::try_from(DetailAndError(detail, error, sites)))
            .collect()
    }
}

#[derive(Queryable, QueryableByName, Identifiable, Associations)]
#[table_name = "subgraph_manifest"]
#[belongs_to(GraphNodeVersion)]
// We never read the id field but map it to make the interaction with Diesel
// simpler
#[allow(dead_code)]
struct StoredSubgraphManifest {
    id: i32,
    spec_version: String,
    description: Option<String>,
    repository: Option<String>,
    features: Vec<String>,
    schema: String,
    graph_node_version_id: i32,
}

impl From<StoredSubgraphManifest> for SubgraphManifestEntity {
    fn from(value: StoredSubgraphManifest) -> Self {
        SubgraphManifestEntity {
            spec_version: value.spec_version,
            description: value.description,
            repository: value.repository,
            features: value.features,
            schema: value.schema,
        }
    }
}

struct StoredDeploymentEntity(crate::detail::DeploymentDetail, StoredSubgraphManifest);

impl TryFrom<StoredDeploymentEntity> for SubgraphDeploymentEntity {
    type Error = StoreError;

    fn try_from(ent: StoredDeploymentEntity) -> Result<Self, Self::Error> {
        let (detail, manifest) = (ent.0, ent.1.into());

        let earliest_block = block(
            &detail.deployment,
            "earliest_block",
            detail.earliest_ethereum_block_hash,
            detail.earliest_ethereum_block_number,
        )?
        .map(|block| block.to_ptr());

        let latest_block = block(
            &detail.deployment,
            "latest_block",
            detail.latest_ethereum_block_hash,
            detail.latest_ethereum_block_number,
        )?
        .map(|block| block.to_ptr());

        let graft_block = block(
            &detail.deployment,
            "graft_block",
            detail.graft_block_hash,
            detail.graft_block_number,
        )?
        .map(|block| block.to_ptr());

        let graft_base = detail
            .graft_base
            .map(|b| DeploymentHash::new(b))
            .transpose()
            .map_err(|b| constraint_violation!("invalid graft base `{}`", b))?;

        Ok(SubgraphDeploymentEntity {
            manifest,
            failed: detail.failed,
            health: detail.health.into(),
            synced: detail.synced,
            fatal_error: None,
            non_fatal_errors: vec![],
            earliest_block,
            latest_block,
            graft_base,
            graft_block,
            reorg_count: detail.reorg_count,
            current_reorg_depth: detail.current_reorg_depth,
            max_reorg_depth: detail.max_reorg_depth,
        })
    }
}

pub fn deployment_entity(
    conn: &PgConnection,
    site: &Site,
) -> Result<SubgraphDeploymentEntity, StoreError> {
    use subgraph_deployment as d;
    use subgraph_manifest as m;

    let manifest = m::table
        .find(site.id)
        .first::<StoredSubgraphManifest>(conn)?;

    let detail = d::table
        .find(site.id)
        .first::<crate::detail::DeploymentDetail>(conn)?;

    SubgraphDeploymentEntity::try_from(StoredDeploymentEntity(detail, manifest))
}

#[derive(Queryable, Identifiable, Insertable)]
#[table_name = "graph_node_versions"]
pub struct GraphNodeVersion {
    pub id: i32,
    pub git_commit_hash: String,
    pub git_repository_dirty: bool,
    pub crate_version: String,
    pub major: i32,
    pub minor: i32,
    pub patch: i32,
    pub pre_release: String,
    pub rustc_version: String,
    pub rustc_host: String,
    pub rustc_channel: String,
}

impl GraphNodeVersion {
    pub(crate) fn create_or_get(conn: &PgConnection) -> anyhow::Result<i32> {
        let git_repository_path =
            GraphNodeVersion::find_git_repository_path(std::path::Path::new(CARGO_MANIFEST_DIR))?;
        let git_repository = Repository::open(&git_repository_path)?;
        let last_commit = GraphNodeVersion::find_last_commit(&git_repository)?;
        let git_commit_hash = last_commit.id().to_string();
        let git_repository_dirty = GraphNodeVersion::is_repository_dirty(&git_repository)?;
        let crate_version = CARGO_PKG_VERSION.to_string();
        let pre_release = CARGO_PKG_VERSION_PRE.to_string();
        let rustc = rustc_version::version_meta()?;
        let rustc_channel = format!("{:?}", rustc.channel);

        // Cargo won't run if those values are not integers, so it is safe to unwrap here.
        let major: i32 = CARGO_PKG_VERSION_MAJOR.parse().unwrap();
        let minor: i32 = CARGO_PKG_VERSION_MINOR.parse().unwrap();
        let patch: i32 = CARGO_PKG_VERSION_PATCH.parse().unwrap();

        // upsert
        let graph_node_version_id = {
            use graph_node_versions::dsl as g;
            diesel::insert_into(g::graph_node_versions)
                .values((
                    g::git_commit_hash.eq(&git_commit_hash),
                    g::git_repository_dirty.eq(git_repository_dirty),
                    g::crate_version.eq(&crate_version),
                    g::major.eq(&major),
                    g::minor.eq(&minor),
                    g::patch.eq(&patch),
                    g::pre_release.eq(&pre_release),
                    g::rustc_version.eq(&rustc.semver.to_string()),
                    g::rustc_host.eq(&rustc.host.to_string()),
                    g::rustc_channel.eq(&rustc_channel),
                ))
                .on_conflict(diesel::pg::upsert::on_constraint(
                    "unique_graph_node_versions",
                ))
                .do_update()
                // inert update to affect and return a row using a single query
                .set(g::id.eq(g::id))
                .returning(g::id)
                .get_result(conn)?
        };
        Ok(graph_node_version_id)
    }

    fn find_git_repository_path(
        starting_path: &std::path::Path,
    ) -> anyhow::Result<std::path::PathBuf> {
        let path = starting_path.join(".git");
        if path.exists() {
            Ok(path)
        } else {
            if let Some(parent) = starting_path.parent() {
                GraphNodeVersion::find_git_repository_path(&parent)
            } else {
                Err(anyhow!("Could not find a git repository."))
            }
        }
    }

    fn find_last_commit(git_repository: &Repository) -> Result<Commit, git2::Error> {
        let obj = git_repository.head()?.resolve()?.peel(ObjectType::Commit)?;
        obj.into_commit()
            .map_err(|_| git2::Error::from_str("Couldn't find commit"))
    }

    fn is_repository_dirty(git_repository: &Repository) -> Result<bool, git2::Error> {
        let mut statuses_options = StatusOptions::new();
        statuses_options.include_ignored(false);
        statuses_options.include_unmodified(false);
        let statuses = git_repository.statuses(Some(&mut statuses_options))?;
        Ok(statuses.iter().next().is_some())
    }
}
