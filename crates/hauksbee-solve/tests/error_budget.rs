use hauksbee_ir::evidence::IntegrationMethod;
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::{run_op, run_tran, Integration, Probe, SolverOptions};

fn divider() -> Circuit {
    let mut circuit = Circuit::new();
    let rail = circuit.node("rail");
    circuit.add(Device::Vsource {
        name: "V1".into(),
        p: rail,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(3.3),
    });
    circuit.add(Device::Resistor {
        name: "R1".into(),
        a: rail,
        b: NodeId::GROUND,
        ohms: 1_000.0,
        tc1: None,
    });
    circuit
}

#[test]
fn operating_point_reports_the_options_used_and_the_measured_kcl_residual() {
    let mut options = SolverOptions::default();
    options.reltol = 2e-4;
    options.vntol = 7e-7;
    options.abstol = 3e-13;
    options.chgtol = 9e-15;

    let output = run_op(&divider(), &options, &[Probe::NodeVoltage("rail".into())]).unwrap();
    let tolerance = output.error_budget.tolerance();
    assert_eq!(tolerance.reltol(), options.reltol);
    assert_eq!(tolerance.vntol(), options.vntol);
    assert_eq!(tolerance.abstol(), options.abstol);
    assert_eq!(tolerance.chgtol(), options.chgtol);

    let residual = output
        .error_budget
        .residual()
        .expect("a Newton operating point measures its accepted KCL residual");
    assert!(residual.max_abs().is_finite());
    assert!(residual.max_abs() < 1e-9, "{residual:?}");
    assert_eq!(residual.at(), "rail");
}

#[test]
fn transient_reports_the_selected_integration_method_and_solved_window() {
    let mut options = SolverOptions::fixed(1e-6);
    options.integration = Integration::Gear2;
    let output = run_tran(
        &divider(),
        &options,
        5e-6,
        &[Probe::NodeVoltage("rail".into())],
    )
    .unwrap();

    assert_eq!(output.error_budget.methods().len(), 1);
    let method = &output.error_budget.methods()[0];
    assert_eq!(method.method(), IntegrationMethod::Gear2);
    assert_eq!(method.window().start_s(), 0.0);
    assert_eq!(method.window().end_s(), 5e-6);
    let residual = output
        .error_budget
        .residual()
        .expect("the final accepted transient equation residual is measured");
    assert!(residual.max_abs().is_finite());
    assert!(residual.max_abs() < 1e-9, "{residual:?}");
}
