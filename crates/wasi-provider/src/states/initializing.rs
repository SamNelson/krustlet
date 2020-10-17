use std::collections::HashMap;

use log::{error, info, warn};

use crate::PodState;
use k8s_openapi::api::core::v1::Pod as KubePod;
use kube::api::{Api, PatchParams};
use kubelet::backoff::BackoffStrategy;
use kubelet::container::{patch_container_status, ContainerKey, Status as ContainerStatus};
use kubelet::pod::{Handle, PodKey};
use kubelet::state::prelude::*;

use super::error::Error;
use super::starting::{start_container, ContainerHandleMap, Starting};
use crate::fail_fatal;

#[derive(Default, Debug, TransitionTo)]
#[transition_to(Starting, Error)]
pub struct Initializing;

#[async_trait::async_trait]
impl State<PodState> for Initializing {
    async fn next(self: Box<Self>, pod_state: &mut PodState, pod: &Pod) -> Transition<PodState> {
        let client: Api<KubePod> = Api::namespaced(
            kube::Client::new(pod_state.shared.kubeconfig.clone()),
            pod.namespace(),
        );
        let mut container_handles: ContainerHandleMap = HashMap::new();

        for init_container in pod.init_containers() {
            info!(
                "Starting init container {:?} for pod {:?}",
                init_container.name(),
                pod.name()
            );

            // Each new init container resets the CrashLoopBackoff timer.
            pod_state.crash_loop_backoff_strategy.reset();

            match start_container(pod_state, pod, &init_container).await {
                Ok(h) => {
                    container_handles
                        .insert(ContainerKey::Init(init_container.name().to_string()), h);
                }
                Err(e) => fail_fatal!(e),
            }

            while let Some((name, status)) = pod_state.run_context.status_recv.recv().await {
                warn!("Container Status Update: {}, {:?}", name, status);
                if let Err(e) = patch_container_status(&client, &pod, &name, &status, true).await {
                    error!("Unable to patch status, will retry on next update: {:?}", e);
                }

                if let ContainerStatus::Terminated {
                    timestamp: _,
                    message,
                    failed,
                } = status
                {
                    if failed {
                        // HACK: update the status message informing which init container failed
                        let s = serde_json::json!({
                            "metadata": {
                                "resourceVersion": "",
                            },
                            "status": {
                                "message": format!("Init container {} failed", name),
                            }
                        });

                        // If we are in a failed state, insert in the init containers we already ran
                        // into a pod handle so they are available for future log fetching
                        let pod_handle = Handle::new(container_handles, pod.clone(), None);
                        let pod_key = PodKey::from(pod);
                        {
                            let mut handles = pod_state.shared.handles.write().await;
                            handles.insert(pod_key, pod_handle);
                        }

                        let status_json = match serde_json::to_vec(&s) {
                            Ok(json) => json,
                            Err(e) => fail_fatal!(e),
                        };

                        match client
                            .patch_status(pod.name(), &PatchParams::default(), status_json)
                            .await
                        {
                            Ok(_) => return Transition::next(self, Error { message }),
                            Err(e) => fail_fatal!(e),
                        };
                    } else {
                        break;
                    }
                }
            }
        }
        info!("Finished init containers for pod {:?}", pod.name());
        pod_state.crash_loop_backoff_strategy.reset();
        Transition::next(self, Starting::new(container_handles))
    }

    async fn json_status(
        &self,
        _pod_state: &mut PodState,
        _pmeod: &Pod,
    ) -> anyhow::Result<serde_json::Value> {
        make_status(Phase::Running, "Initializing")
    }
}
