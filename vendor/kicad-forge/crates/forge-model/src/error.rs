use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("parse error: {0}")]
    Parse(#[from] forge_sexpr::ParseError),

    #[error("not a kicad_pcb file: root node is '{0}'")]
    NotPcb(String),

    #[error("not a kicad_sch file: root node is '{0}'")]
    NotSchematic(String),

    #[error("missing root node")]
    MissingRoot,

    #[error("footprint '{0}' not found")]
    FootprintNotFound(String),
}
