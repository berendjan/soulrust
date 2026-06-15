//! Exercises the `messenger!` macro with the same topology shapes the Go
//! interface-gen-v2/messenger-gen-v2 tools support: 1:1 request/response,
//! event fan-out to multiple handlers, events with no handlers, and the
//! `Proto` suffix stripping in derived method names.

use messenger_macro::messenger;

pub mod pb {
    pub struct GetTenantRequest {
        pub tenant_id: u32,
    }

    pub struct GetTenantResponse {
        pub name: String,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TenantUpdatedProto {
    pub tenant_id: u32,
}

#[derive(Clone)]
pub struct WorkerDone {
    pub job: u32,
}

#[derive(Clone)]
pub struct Telemetry {
    pub event: &'static str,
}

messenger! {
    messenger UnitMessenger;

    component controller {
        sends pb::GetTenantRequest -> pb::GetTenantResponse;
        sends TenantUpdatedProto;
        sends Telemetry;
        handles WorkerDone;
    }

    component worker {
        handles pb::GetTenantRequest -> pb::GetTenantResponse;
        handles TenantUpdatedProto;
        sends WorkerDone;
    }

    component audit {
        handles TenantUpdatedProto;
    }
}

#[derive(Default)]
pub struct Controller {
    pub done_jobs: Vec<u32>,
}

impl ControllerHandler for Controller {
    fn handle_worker_done(&mut self, msg: WorkerDone) {
        self.done_jobs.push(msg.job);
    }
}

#[derive(Default)]
pub struct Worker {
    pub seen_tenants: Vec<u32>,
}

impl WorkerHandler for Worker {
    fn handle_get_tenant_request(&mut self, msg: pb::GetTenantRequest) -> pb::GetTenantResponse {
        pb::GetTenantResponse {
            name: format!("tenant-{}", msg.tenant_id),
        }
    }

    fn handle_tenant_updated(&mut self, msg: TenantUpdatedProto) {
        self.seen_tenants.push(msg.tenant_id);
    }
}

#[derive(Default)]
pub struct Audit {
    pub events: Vec<TenantUpdatedProto>,
}

impl AuditHandler for Audit {
    fn handle_tenant_updated(&mut self, msg: TenantUpdatedProto) {
        self.events.push(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_messenger() -> UnitMessenger<Controller, Worker, Audit> {
        UnitMessenger::new(Controller::default(), Worker::default(), Audit::default())
    }

    #[test]
    fn request_response_routes_to_single_handler() {
        let mut m = new_messenger();
        let response = m
            .controller_sendable()
            .send_get_tenant_request(pb::GetTenantRequest { tenant_id: 7 });
        assert_eq!(response.name, "tenant-7");
    }

    #[test]
    fn event_fans_out_to_all_handlers() {
        let mut m = new_messenger();
        // `TenantUpdatedProto` -> `send_tenant_updated`: the `Proto` suffix
        // is stripped from derived method names, like the Go baseName helper.
        m.controller_sendable()
            .send_tenant_updated(TenantUpdatedProto { tenant_id: 3 });
        assert_eq!(m.worker.seen_tenants, vec![3]);
        assert_eq!(m.audit.events, vec![TenantUpdatedProto { tenant_id: 3 }]);
    }

    #[test]
    fn event_without_handlers_is_dropped() {
        let mut m = new_messenger();
        m.controller_sendable().send_telemetry(Telemetry { event: "boot" });
    }

    #[test]
    fn event_routes_back_to_controller() {
        let mut m = new_messenger();
        m.worker_sendable().send_worker_done(WorkerDone { job: 42 });
        assert_eq!(m.controller.done_jobs, vec![42]);
    }

    // A component generic over its sendable, showing how component code uses
    // the generated trait without the messenger type appearing in it — the
    // Rust replacement for Go's SetSendable.
    fn run_controller_logic(sendable: &mut impl ControllerSendable) -> String {
        sendable
            .send_get_tenant_request(pb::GetTenantRequest { tenant_id: 1 })
            .name
    }

    #[test]
    fn components_depend_only_on_their_sendable_trait() {
        let mut m = new_messenger();
        assert_eq!(run_controller_logic(&mut m.controller_sendable()), "tenant-1");
    }
}
