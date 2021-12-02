//! Ensures that `Pod`s are configured and running for each [`ZookeeperCluster`]

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    time::Duration,
};

use crate::utils::apply_owned;
use snafu::{OptionExt, ResultExt, Snafu};
use stackable_operator::{
    builder::{ConfigMapBuilder, ContainerBuilder, ObjectMetaBuilder, PodBuilder},
    k8s_openapi::{
        api::{
            apps::v1::{StatefulSet, StatefulSetSpec},
            core::v1::{
                ConfigMap, ConfigMapVolumeSource, EnvVar, EnvVarSource, ExecAction,
                ObjectFieldSelector, PersistentVolumeClaim, PersistentVolumeClaimSpec, Probe,
                ResourceRequirements, Service, ServicePort, ServiceSpec, Volume,
            },
        },
        apimachinery::pkg::{api::resource::Quantity, apis::meta::v1::LabelSelector},
    },
    kube::{
        self,
        api::ObjectMeta,
        runtime::{
            controller::{Context, ReconcilerAction},
            reflector::ObjectRef,
        },
    },
    labels::{role_group_selector_labels, role_selector_labels},
    product_config::{
        types::PropertyNameKind, writer::to_java_properties_string, ProductConfigManager,
    },
    product_config_utils::{transform_all_roles_to_config, validate_all_roles_and_groups_config},
};
use stackable_zookeeper_crd::{RoleGroupRef, ZookeeperCluster, ZookeeperRole};

const FIELD_MANAGER: &str = "zookeeper.stackable.tech/zookeepercluster";
const APP_NAME: &str = "zookeeper";

pub struct Ctx {
    pub kube: kube::Client,
    pub product_config: ProductConfigManager,
}

#[derive(Snafu, Debug)]
#[allow(clippy::enum_variant_names)]
pub enum Error {
    #[snafu(display("object {} has no namespace", obj_ref))]
    ObjectHasNoNamespace {
        obj_ref: ObjectRef<ZookeeperCluster>,
    },
    #[snafu(display("object {} defines no version", obj_ref))]
    ObjectHasNoVersion {
        obj_ref: ObjectRef<ZookeeperCluster>,
    },
    #[snafu(display("failed to calculate global service name for {}", obj_ref))]
    GlobalServiceNameNotFound {
        obj_ref: ObjectRef<ZookeeperCluster>,
    },
    #[snafu(display("failed to calculate service name for role {}", rolegroup))]
    RoleGroupServiceNameNotFound { rolegroup: RoleGroupRef },
    #[snafu(display("failed to apply global Service for {}", zk))]
    ApplyGlobalService {
        source: kube::Error,
        zk: ObjectRef<ZookeeperCluster>,
    },
    #[snafu(display("failed to apply Service for {}", rolegroup))]
    ApplyRoleGroupService {
        source: kube::Error,
        rolegroup: RoleGroupRef,
    },
    #[snafu(display("failed to build ConfigMap for {}", rolegroup))]
    BuildRoleGroupConfig {
        source: stackable_operator::error::Error,
        rolegroup: RoleGroupRef,
    },
    #[snafu(display("failed to apply ConfigMap for {}", rolegroup))]
    ApplyRoleGroupConfig {
        source: kube::Error,
        rolegroup: RoleGroupRef,
    },
    #[snafu(display("failed to apply StatefulSet for {}", rolegroup))]
    ApplyRoleGroupStatefulSet {
        source: kube::Error,
        rolegroup: RoleGroupRef,
    },
    #[snafu(display("invalid product config for {}", zk))]
    InvalidProductConfig {
        source: stackable_operator::error::Error,
        zk: ObjectRef<ZookeeperCluster>,
    },
    #[snafu(display("failed to serialize zoo.cfg for {}", rolegroup))]
    SerializeZooCfg {
        source: stackable_operator::product_config::writer::PropertiesWriterError,
        rolegroup: RoleGroupRef,
    },
    #[snafu(display("object {} is missing metadata to build owner reference", zk))]
    ObjectMissingMetadataForOwnerRef {
        source: stackable_operator::error::Error,
        zk: ObjectRef<ZookeeperCluster>,
    },
}
type Result<T, E = Error> = std::result::Result<T, E>;

const PROPERTIES_FILE: &str = "zoo.cfg";

pub async fn reconcile_zk(zk: ZookeeperCluster, ctx: Context<Ctx>) -> Result<ReconcilerAction> {
    tracing::info!("Starting reconcile");
    let zk_ref = ObjectRef::from_obj(&zk);
    let kube = ctx.get_ref().kube.clone();

    let zk_version = zk
        .spec
        .version
        .as_deref()
        .with_context(|| ObjectHasNoVersion {
            obj_ref: zk_ref.clone(),
        })?;
    let validated_config = validate_all_roles_and_groups_config(
        zk_version,
        &transform_all_roles_to_config(
            &zk,
            [(
                ZookeeperRole::Server.to_string(),
                (
                    vec![
                        PropertyNameKind::Env,
                        PropertyNameKind::File(PROPERTIES_FILE.to_string()),
                    ],
                    zk.spec.servers.clone(),
                ),
            )]
            .into(),
        ),
        &ctx.get_ref().product_config,
        false,
        false,
    )
    .with_context(|| InvalidProductConfig { zk: zk_ref.clone() })?;
    let role_server_config = validated_config
        .get(&ZookeeperRole::Server.to_string())
        .map(Cow::Borrowed)
        .unwrap_or_default();

    apply_owned(&kube, FIELD_MANAGER, &build_server_role_service(&zk)?)
        .await
        .with_context(|| ApplyGlobalService { zk: zk_ref.clone() })?;
    for (rolegroup_name, rolegroup_config) in role_server_config.iter() {
        let rolegroup = zk.server_rolegroup_ref(rolegroup_name);

        apply_owned(
            &kube,
            FIELD_MANAGER,
            &build_server_rolegroup_service(&rolegroup, &zk)?,
        )
        .await
        .with_context(|| ApplyRoleGroupService {
            rolegroup: rolegroup.clone(),
        })?;
        apply_owned(
            &kube,
            FIELD_MANAGER,
            &build_server_rolegroup_config_map(&rolegroup, &zk, rolegroup_config)?,
        )
        .await
        .with_context(|| ApplyRoleGroupConfig {
            rolegroup: rolegroup.clone(),
        })?;
        apply_owned(
            &kube,
            FIELD_MANAGER,
            &build_server_rolegroup_statefulset(&rolegroup, &zk, rolegroup_config)?,
        )
        .await
        .with_context(|| ApplyRoleGroupStatefulSet {
            rolegroup: rolegroup.clone(),
        })?;
    }

    Ok(ReconcilerAction {
        requeue_after: None,
    })
}

/// The server-role service is the primary endpoint that should be used by clients that do not perform internal load balancing,
/// including targets outside of the cluster.
///
/// Note that you should generally *not* hard-code clients to use these services; instead, create a [`ZookeeperZnode`](`stackable_zookeeper_crd::ZookeeperZnode`)
/// and use the connection string that it gives you.
pub fn build_server_role_service(zk: &ZookeeperCluster) -> Result<Service> {
    let role_name = ZookeeperRole::Server.to_string();
    let role_svc_name =
        zk.server_role_service_name()
            .with_context(|| GlobalServiceNameNotFound {
                obj_ref: ObjectRef::from_obj(zk),
            })?;
    Ok(Service {
        metadata: ObjectMetaBuilder::new()
            .name_and_namespace(zk)
            .name(&role_svc_name)
            .ownerreference_from_resource(zk, None, Some(true))
            .with_context(|| ObjectMissingMetadataForOwnerRef {
                zk: ObjectRef::from_obj(zk),
            })?
            .with_recommended_labels(zk, APP_NAME, zk_version(zk)?, &role_name, "global")
            .build(),
        spec: Some(ServiceSpec {
            ports: Some(vec![ServicePort {
                name: Some("zk".to_string()),
                port: 2181,
                protocol: Some("TCP".to_string()),
                ..ServicePort::default()
            }]),
            selector: Some(role_selector_labels(zk, APP_NAME, &role_name)),
            type_: Some("NodePort".to_string()),
            ..ServiceSpec::default()
        }),
        status: None,
    })
}

/// The rolegroup [`ConfigMap`] configures the rolegroup based on the configuration given by the administrator
fn build_server_rolegroup_config_map(
    rolegroup: &RoleGroupRef,
    zk: &ZookeeperCluster,
    server_config: &HashMap<PropertyNameKind, BTreeMap<String, String>>,
) -> Result<ConfigMap> {
    let mut zoo_cfg = server_config
        .get(&PropertyNameKind::File(PROPERTIES_FILE.to_string()))
        .cloned()
        .unwrap_or_default();
    zoo_cfg.insert("dataDir".to_string(), "/stackable/data".to_string());
    zoo_cfg.insert("clientPort".to_string(), "2181".to_string());
    zoo_cfg.extend(zk.pods().into_iter().flatten().map(|pod| {
        (
            format!("server.{}", pod.zookeeper_id),
            format!("{}:2888:3888;2181", pod.fqdn()),
        )
    }));
    let zoo_cfg = zoo_cfg
        .into_iter()
        .map(|(k, v)| (k, Some(v)))
        .collect::<Vec<_>>();
    ConfigMapBuilder::new()
        .metadata(
            ObjectMetaBuilder::new()
                .name_and_namespace(zk)
                .name(rolegroup.object_name())
                .ownerreference_from_resource(zk, None, Some(true))
                .with_context(|| ObjectMissingMetadataForOwnerRef {
                    zk: ObjectRef::from_obj(zk),
                })?
                .with_recommended_labels(
                    zk,
                    APP_NAME,
                    zk_version(zk)?,
                    &rolegroup.role,
                    &rolegroup.role_group,
                )
                .build(),
        )
        .add_data(
            "zoo.cfg",
            to_java_properties_string(zoo_cfg.iter().map(|(k, v)| (k, v))).with_context(|| {
                SerializeZooCfg {
                    rolegroup: rolegroup.clone(),
                }
            })?,
        )
        .build()
        .with_context(|| BuildRoleGroupConfig {
            rolegroup: rolegroup.clone(),
        })
}

/// The rolegroup [`Service`] is a headless service that allows direct access to the instances of a certain rolegroup
///
/// This is mostly useful for internal communication between peers, or for clients that perform client-side load balancing.
fn build_server_rolegroup_service(
    rolegroup: &RoleGroupRef,
    zk: &ZookeeperCluster,
) -> Result<Service> {
    Ok(Service {
        metadata: ObjectMetaBuilder::new()
            .name_and_namespace(zk)
            .name(&rolegroup.object_name())
            .ownerreference_from_resource(zk, None, Some(true))
            .with_context(|| ObjectMissingMetadataForOwnerRef {
                zk: ObjectRef::from_obj(zk),
            })?
            .with_recommended_labels(
                zk,
                APP_NAME,
                zk_version(zk)?,
                &rolegroup.role,
                &rolegroup.role_group,
            )
            .build(),
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".to_string()),
            ports: Some(vec![ServicePort {
                name: Some("zk".to_string()),
                port: 2181,
                protocol: Some("TCP".to_string()),
                ..ServicePort::default()
            }]),
            selector: Some(role_group_selector_labels(
                zk,
                APP_NAME,
                &rolegroup.role,
                &rolegroup.role_group,
            )),
            publish_not_ready_addresses: Some(true),
            ..ServiceSpec::default()
        }),
        status: None,
    })
}

/// The rolegroup [`StatefulSet`] runs the rolegroup, as configured by the administrator.
///
/// The [`Pod`](`stackable_operator::k8s_openapi::api::core::v1::Pod`)s are accessible through the corresponding [`Service`] (from [`build_rolegroup_service`]).
fn build_server_rolegroup_statefulset(
    rolegroup_ref: &RoleGroupRef,
    zk: &ZookeeperCluster,
    server_config: &HashMap<PropertyNameKind, BTreeMap<String, String>>,
) -> Result<StatefulSet> {
    let rolegroup = zk.spec.servers.role_groups.get(&rolegroup_ref.role_group);
    let zk_version = zk_version(zk)?;
    let image = format!(
        "docker.stackable.tech/stackable/zookeeper:{}-stackable0",
        zk_version
    );
    let env = server_config
        .get(&PropertyNameKind::Env)
        .iter()
        .flat_map(|env_vars| env_vars.iter())
        .map(|(k, v)| EnvVar {
            name: k.clone(),
            value: Some(v.clone()),
            ..EnvVar::default()
        })
        .collect::<Vec<_>>();
    let container_decide_myid = ContainerBuilder::new("decide-myid")
        .image(&image)
        .args(vec![
            "sh".to_string(),
            "-c".to_string(),
            "expr $MYID_OFFSET + $(echo $POD_NAME | sed 's/.*-//') > /stackable/data/myid"
                .to_string(),
        ])
        .add_env_vars(env.clone())
        .add_env_vars(vec![EnvVar {
            name: "POD_NAME".to_string(),
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector {
                    api_version: Some("v1".to_string()),
                    field_path: "metadata.name".to_string(),
                }),
                ..EnvVarSource::default()
            }),
            ..EnvVar::default()
        }])
        .add_volume_mount("data", "/stackable/data")
        .build();
    let container_zk = ContainerBuilder::new("zookeeper")
        .image(image)
        .args(vec![
            "bin/zkServer.sh".to_string(),
            "start-foreground".to_string(),
            "/stackable/config/zoo.cfg".to_string(),
        ])
        .add_env_vars(env)
        // Only allow the global load balancing service to send traffic to pods that are members of the quorum
        // This also acts as a hint to the StatefulSet controller to wait for each pod to enter quorum before taking down the next
        .readiness_probe(Probe {
            exec: Some(ExecAction {
                command: Some(vec![
                    "bash".to_string(),
                    "-c".to_string(),
                    // We don't have telnet or netcat in the container images, but
                    // we can use Bash's virtual /dev/tcp filesystem to accomplish the same thing
                    "exec 3<>/dev/tcp/localhost/2181 && echo srvr >&3 && grep '^Mode: ' <&3"
                        .to_string(),
                ]),
            }),
            period_seconds: Some(1),
            ..Probe::default()
        })
        .add_container_port("zk", 2181)
        .add_container_port("zk-leader", 2888)
        .add_container_port("zk-election", 3888)
        .add_volume_mount("data", "/stackable/data")
        .add_volume_mount("config", "/stackable/config")
        .build();
    Ok(StatefulSet {
        metadata: ObjectMetaBuilder::new()
            .name_and_namespace(zk)
            .name(&rolegroup_ref.object_name())
            .ownerreference_from_resource(zk, None, Some(true))
            .with_context(|| ObjectMissingMetadataForOwnerRef {
                zk: ObjectRef::from_obj(zk),
            })?
            .with_recommended_labels(
                zk,
                APP_NAME,
                zk_version,
                &rolegroup_ref.role,
                &rolegroup_ref.role_group,
            )
            .build(),
        spec: Some(StatefulSetSpec {
            pod_management_policy: Some("Parallel".to_string()),
            replicas: if zk.spec.stopped.unwrap_or(false) {
                Some(0)
            } else {
                rolegroup.and_then(|rg| rg.replicas).map(i32::from)
            },
            selector: LabelSelector {
                match_labels: Some(role_group_selector_labels(
                    zk,
                    APP_NAME,
                    &rolegroup_ref.role,
                    &rolegroup_ref.role_group,
                )),
                ..LabelSelector::default()
            },
            service_name: rolegroup_ref.object_name(),
            template: PodBuilder::new()
                .metadata_builder(|m| {
                    m.with_recommended_labels(
                        zk,
                        APP_NAME,
                        zk_version,
                        &rolegroup_ref.role,
                        &rolegroup_ref.role_group,
                    )
                })
                .add_init_container(container_decide_myid)
                .add_container(container_zk)
                .add_volume(Volume {
                    name: "config".to_string(),
                    config_map: Some(ConfigMapVolumeSource {
                        name: Some(rolegroup_ref.object_name()),
                        ..ConfigMapVolumeSource::default()
                    }),
                    ..Volume::default()
                })
                .build_template(),
            volume_claim_templates: Some(vec![PersistentVolumeClaim {
                metadata: ObjectMeta {
                    name: Some("data".to_string()),
                    ..ObjectMeta::default()
                },
                spec: Some(PersistentVolumeClaimSpec {
                    access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                    resources: Some(ResourceRequirements {
                        requests: Some({
                            let mut map = BTreeMap::new();
                            map.insert("storage".to_string(), Quantity("1Gi".to_string()));
                            map
                        }),
                        ..ResourceRequirements::default()
                    }),
                    ..PersistentVolumeClaimSpec::default()
                }),
                ..PersistentVolumeClaim::default()
            }]),
            ..StatefulSetSpec::default()
        }),
        status: None,
    })
}

fn zk_version(zk: &ZookeeperCluster) -> Result<&str> {
    zk.spec
        .version
        .as_deref()
        .with_context(|| ObjectHasNoVersion {
            obj_ref: ObjectRef::from_obj(zk),
        })
}

pub fn error_policy(_error: &Error, _ctx: Context<Ctx>) -> ReconcilerAction {
    ReconcilerAction {
        requeue_after: Some(Duration::from_secs(5)),
    }
}
