use anyhow::{Context, Result};
use comfy_table::presets::UTF8_FULL;
use comfy_table::*;
use libblkcapt::{
    model::entities::{FeatureState, ScheduleModel},
    parsing::parse_uuid,
};
use presets::ASCII_NO_BORDERS;
use std::{convert::TryInto, str::FromStr};
use uuid::Uuid;

pub fn print_comfy_table(header: Vec<Cell>, rows: impl Iterator<Item = Vec<Cell>>) {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(header);

    rows.for_each(|r| {
        table.add_row(r);
    });

    println!("{}", table);
}

pub fn comfy_feature_state_cell(state: FeatureState) -> Cell {
    Cell::new(state).fg(match state {
        FeatureState::Enabled => comfy_table::Color::Green,
        FeatureState::Paused => comfy_table::Color::Yellow,
        FeatureState::Unconfigured => comfy_table::Color::Red,
    })
}

pub fn comfy_id_header() -> Cell {
    comfy_identifier_header("ID")
}

pub fn comfy_index_header() -> Cell {
    comfy_identifier_header("Index")
}

pub fn comfy_identifier_header(name: &str) -> Cell {
    Cell::new(name).add_attribute(Attribute::Bold)
}

pub fn comfy_id_value<T: Into<Uuid>>(uuid: T) -> Cell {
    Cell::new(&uuid.into().to_string()[0..8])
        .fg(Color::Blue)
        .add_attribute(Attribute::Bold)
}

pub fn comfy_id_value_full<T: Into<Uuid>>(uuid: T) -> Cell {
    Cell::new(&uuid.into().to_string())
        .fg(Color::Blue)
        .add_attribute(Attribute::Bold)
}

pub fn comfy_name_value<T: ToString>(name: T) -> Cell {
    Cell::new(name).fg(Color::Blue)
}

pub fn comfy_value_or<T: ToString>(value: Option<T>, default: &str) -> Cell {
    value
        .map(|v| Cell::new(v.to_string()))
        .unwrap_or_else(|| Cell::new(default))
}

pub enum CellOrCells {
    Cell(Cell),
    Cells(Vec<Cell>),
}

impl From<Cell> for CellOrCells {
    fn from(cell: Cell) -> Self {
        Self::Cell(cell)
    }
}

impl From<Vec<Cell>> for CellOrCells {
    fn from(cells: Vec<Cell>) -> Self {
        Self::Cells(cells)
    }
}

pub fn print_comfy_info(rows: Vec<(Cell, CellOrCells)>) {
    let mut table = Table::new();
    table
        .load_preset(ASCII_NO_BORDERS)
        .remove_style(TableComponent::HorizontalLines)
        .remove_style(TableComponent::VerticalLines)
        .remove_style(TableComponent::MiddleIntersections)
        .set_content_arrangement(ContentArrangement::Dynamic);

    for (header, value) in rows {
        match value {
            CellOrCells::Cell(cell) => {
                table.add_row(vec![header, cell]);
            }
            CellOrCells::Cells(cells) => {
                let mut cell_iter = cells.into_iter();
                table.add_row(vec![header, cell_iter.next().unwrap_or_else(|| Cell::new(""))]);
                cell_iter.for_each(|c| {
                    table.add_row(vec![Cell::new(""), c]);
                });
            }
        }
    }

    println!("{}", table);
}

#[derive(Debug)]
pub struct UuidArg(Uuid);

impl UuidArg {
    pub fn uuid(&self) -> Uuid {
        self.0
    }

    pub fn parse(s: &str) -> Result<Uuid> {
        Self::from_str(s).map(|arg| arg.uuid())
    }
}

impl FromStr for UuidArg {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_uuid(s).map(UuidArg)
    }
}

#[derive(Debug, Clone)]
pub struct ScheduleArg(ScheduleModel);

impl ScheduleArg {
    pub fn into_schedule_model(self) -> ScheduleModel {
        self.0
    }
}

impl From<ScheduleArg> for ScheduleModel {
    fn from(arg: ScheduleArg) -> Self {
        arg.into_schedule_model()
    }
}

impl FromStr for ScheduleArg {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if s.contains(' ') {
            s.parse().map(Self)
        } else {
            let duration = *(s.parse::<humantime::Duration>()?);
            duration.try_into().map(Self).context(
                "The specified frequency can't be converted into a schedule. \
            For more advanced schedule creation use a cron expression. \
            See --help for more details.",
            )
        }
    }
}
