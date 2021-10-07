use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::{
    api::core::v1::{
        LocalVolumeSource, NodeSelector, NodeSelectorRequirement, NodeSelectorTerm,
        ObjectReference, PersistentVolume, PersistentVolumeClaim, PersistentVolumeSpec,
        VolumeNodeAffinity,
    },
    apimachinery::pkg::api::resource::Quantity,
};
use kube::{
    api::{ListParams, ObjectMeta, Patch, PatchParams},
    Api, Resource,
};
use kube_runtime::{
    controller::{self, ReconcilerAction},
    Controller,
};
use snafu::{OptionExt, ResultExt, Snafu};

#[derive(Debug, Snafu)]
enum Error {
    UnbindablePvc,
    CreatePvFailed { source: kube::Error },
}

struct BindablePvc<'a> {
    node_name: &'a str,
    uid: &'a str,
    storage_class: &'a str,
}

impl<'a> BindablePvc<'a> {
    fn from(pvc: &'a PersistentVolumeClaim) -> Option<Self> {
        let spec = pvc.spec.as_ref()?;
        if spec.volume_name.is_some() {
            return None;
        }
        let annotations = pvc.metadata.annotations.as_ref()?;
        Some(Self {
            node_name: annotations.get("volume.kubernetes.io/selected-node")?,
            uid: pvc.metadata.uid.as_deref()?,
            storage_class: spec.storage_class_name.as_deref()?,
        })
    }
}

struct Ctx {
    pvs: Api<PersistentVolume>,
    // nodes: Api<Node>,
}

fn object_ref_to<K: Resource<DynamicType = ()>>(obj: &K) -> ObjectReference {
    ObjectReference {
        api_version: Some(K::api_version(&()).into_owned()),
        kind: Some(K::kind(&()).into_owned()),
        name: obj.meta().name.clone(),
        namespace: obj.meta().namespace.clone(),
        resource_version: obj.meta().resource_version.clone(),
        uid: obj.meta().uid.clone(),
        ..ObjectReference::default()
    }
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let kube = kube::Client::try_default().await?;
    let pvcs = Api::<PersistentVolumeClaim>::all(kube.clone());
    Controller::new(pvcs, ListParams::default())
        .run(
            |pvc, ctx| async move {
                let pvc_ref = object_ref_to(&pvc);
                let pvc = BindablePvc::from(&pvc).context(UnbindablePvc)?;
                let pv_name = format!("pvc-{}", pvc.uid);
                ctx.get_ref()
                    .pvs
                    .patch(
                        &pv_name,
                        &PatchParams {
                            force: true,
                            field_manager: Some("stackable-pvc-provisioner".to_string()),
                            ..PatchParams::default()
                        },
                        &Patch::Apply(PersistentVolume {
                            metadata: ObjectMeta {
                                name: Some(pv_name.clone()),
                                ..ObjectMeta::default()
                            },
                            spec: Some(PersistentVolumeSpec {
                                access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                                capacity: Some(
                                    std::array::IntoIter::new([(
                                        "storage".to_string(),
                                        Quantity("1Gi".to_string()),
                                    )])
                                    .collect(),
                                ),
                                local: Some(LocalVolumeSource {
                                    path: format!("/var/lib/stackable/volumes/{}", pv_name),
                                    ..LocalVolumeSource::default()
                                }),
                                node_affinity: Some(VolumeNodeAffinity {
                                    required: Some(NodeSelector {
                                        node_selector_terms: vec![NodeSelectorTerm {
                                            match_expressions: Some(vec![
                                                NodeSelectorRequirement {
                                                    key: "kubernetes.io/hostname".to_string(),
                                                    operator: "In".to_string(),
                                                    values: Some(vec![pvc.node_name.to_string()]),
                                                },
                                            ]),
                                            ..NodeSelectorTerm::default()
                                        }],
                                    }),
                                }),
                                claim_ref: Some(pvc_ref),
                                persistent_volume_reclaim_policy: Some("Delete".to_string()),
                                storage_class_name: Some(pvc.storage_class.to_string()),
                                ..PersistentVolumeSpec::default()
                            }),
                            ..PersistentVolume::default()
                        }),
                    )
                    .await
                    .context(CreatePvFailed)?;
                Ok(ReconcilerAction {
                    requeue_after: None,
                })
            },
            |_: &Error, _| ReconcilerAction {
                requeue_after: Some(Duration::from_secs(5)),
            },
            controller::Context::new(Ctx {
                pvs: Api::<PersistentVolume>::all(kube.clone()),
                // nodes: Api::<Node>::all(kube),
            }),
        )
        .for_each(|res| {
            match res {
                Ok((obj, _)) => println!("reconciler {}", obj),
                Err(err) => println!("{}", color_eyre::Report::from(err)),
            }
            async {}
        })
        .await;
    Ok(())
}
