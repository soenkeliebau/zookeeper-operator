use std::{collections::BTreeSet, num::TryFromIntError};

use snafu::{OptionExt, ResultExt, Snafu};
use stackable_operator::{
    builder::{ConfigMapBuilder, ObjectMetaBuilder},
    k8s_openapi::api::core::v1::{ConfigMap, Endpoints, Service},
    kube::{runtime::reflector::ObjectRef, Resource, ResourceExt},
};
use stackable_zookeeper_crd::{ZookeeperCluster, ZookeeperRole};

use crate::{zk_controller::zk_version, APP_NAME, APP_PORT};

#[derive(Snafu, Debug)]
pub enum Error {
    #[snafu(display("object {} is missing metadata to build owner reference", zk))]
    ObjectMissingMetadataForOwnerRef {
        source: stackable_operator::error::Error,
        zk: ObjectRef<ZookeeperCluster>,
    },
    #[snafu(display("chroot path {} was relative (must be absolute)", chroot))]
    RelativeChroot { chroot: String },
    #[snafu(display("object has no name associated"))]
    NoName,
    #[snafu(display("object has no namespace associated"))]
    NoNamespace,
    #[snafu(display("failed to list expected pods"))]
    ExpectedPods {
        source: stackable_zookeeper_crd::NoNamespaceError,
    },
    #[snafu(display("could not find service port with name {}", port_name))]
    NoServicePort { port_name: String },
    #[snafu(display("service port with name {} does not have a nodePort", port_name))]
    NoNodePort { port_name: String },
    #[snafu(display("could not find Endpoints for {}", svc))]
    FindEndpoints {
        source: stackable_operator::error::Error,
        svc: ObjectRef<Service>,
    },
    #[snafu(display("nodePort was out of range"))]
    InvalidNodePort { source: TryFromIntError },
    #[snafu(display("failed to build ConfigMap"))]
    BuildConfigMap {
        source: stackable_operator::error::Error,
    },
}

/// Builds discovery [`ConfigMap`]s for connecting to a [`ZookeeperCluster`] for all expected scenarios
pub async fn build_discovery_configmaps(
    client: &stackable_operator::client::Client,
    owner: &impl Resource<DynamicType = ()>,
    zk: &ZookeeperCluster,
    svc: &Service,
    chroot: Option<&str>,
) -> Result<Vec<ConfigMap>, Error> {
    let name = owner.name();
    Ok(vec![
        build_discovery_configmap(&name, owner, zk, chroot, pod_hosts(zk)?)?,
        build_discovery_configmap(
            &format!("{}-nodeport", name),
            owner,
            zk,
            chroot,
            nodeport_hosts(client, svc, "zk").await?,
        )?,
    ])
}

/// Build a discovery [`ConfigMap`] containing information about how to connect to a certain [`ZookeeperCluster`]
///
/// `hosts` will usually come from either [`pod_hosts`] or [`nodeport_hosts`].
fn build_discovery_configmap(
    name: &str,
    owner: &impl Resource<DynamicType = ()>,
    zk: &ZookeeperCluster,
    chroot: Option<&str>,
    hosts: impl IntoIterator<Item = (impl Into<String>, u16)>,
) -> Result<ConfigMap, Error> {
    // Write a connection string of the format that Java ZooKeeper client expects:
    // "{host1}:{port1},{host2:port2},.../{chroot}"
    // See https://zookeeper.apache.org/doc/current/apidocs/zookeeper-server/org/apache/zookeeper/ZooKeeper.html#ZooKeeper-java.lang.String-int-org.apache.zookeeper.Watcher-
    let mut conn_str = hosts
        .into_iter()
        .map(|(host, port)| format!("{}:{}", host.into(), port))
        .collect::<Vec<_>>()
        .join(",");
    if let Some(chroot) = chroot {
        if !chroot.starts_with('/') {
            return RelativeChrootSnafu { chroot }.fail();
        }
        conn_str.push_str(chroot);
    }
    ConfigMapBuilder::new()
        .metadata(
            ObjectMetaBuilder::new()
                .name_and_namespace(zk)
                .name(name)
                .ownerreference_from_resource(owner, None, Some(true))
                .with_context(|_| ObjectMissingMetadataForOwnerRefSnafu {
                    zk: ObjectRef::from_obj(zk),
                })?
                .with_recommended_labels(
                    zk,
                    APP_NAME,
                    zk_version(zk).unwrap_or("unknown"),
                    &ZookeeperRole::Server.to_string(),
                    "discovery",
                )
                .build(),
        )
        .add_data("ZOOKEEPER", conn_str)
        .build()
        .context(BuildConfigMapSnafu)
}

/// Lists all Pods FQDNs expected to host the [`ZookeeperCluster`]
fn pod_hosts(zk: &ZookeeperCluster) -> Result<impl IntoIterator<Item = (String, u16)> + '_, Error> {
    Ok(zk
        .pods()
        .context(ExpectedPodsSnafu)?
        .into_iter()
        .map(|pod_ref| (pod_ref.fqdn(), APP_PORT)))
}

/// Lists all nodes currently hosting Pods participating in the [`Service`]
async fn nodeport_hosts(
    client: &stackable_operator::client::Client,
    svc: &Service,
    port_name: &str,
) -> Result<impl IntoIterator<Item = (String, u16)>, Error> {
    let svc_port = svc
        .spec
        .as_ref()
        .and_then(|svc_spec| {
            svc_spec
                .ports
                .as_ref()?
                .iter()
                .find(|port| port.name.as_deref() == Some(port_name))
        })
        .context(NoServicePortSnafu { port_name })?;
    let node_port = svc_port.node_port.context(NoNodePortSnafu { port_name })?;
    let endpoints = client
        .get::<Endpoints>(
            svc.metadata.name.as_deref().context(NoNameSnafu)?,
            svc.metadata.namespace.as_deref(),
        )
        .await
        .with_context(|_| FindEndpointsSnafu {
            svc: ObjectRef::from_obj(svc),
        })?;
    let nodes = endpoints
        .subsets
        .into_iter()
        .flatten()
        .flat_map(|subset| subset.addresses)
        .flatten()
        .flat_map(|addr| addr.node_name);
    let addrs = nodes
        .map(|node| Ok((node, node_port.try_into().context(InvalidNodePortSnafu)?)))
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(addrs)
}
