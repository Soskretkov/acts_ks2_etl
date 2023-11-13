use crate::constants::XL_FILE_EXTENSION;
use crate::errors::Error;
use crate::ui;
use calamine::{DataType, Range, Reader, Xlsx, XlsxError};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use walkdir::WalkDir;

#[derive(PartialEq)]
pub enum Required {
    Y,
    N,
}

// В кортеже первое значение это значение валидации смещения от первого столбца до столбца,
// содержащего указанный литерал. Представление в виде "магических цифр" удобнее конфигурировать.
// Литералы маленькими буквами из-за перестраховки от сюрпризов т.е. это поисковые теги,
// например, "Доп. соглашение" Excel переведет в "Доп. Соглашение" если встать в ячейку и нажать Enter.
pub const SEARCH_REFERENCE_POINTS: [(usize, Required, &str); 10] = [
    (0, Required::N, "исполнитель"),
    (0, Required::Y, "стройка"),
    (0, Required::Y, "объект"),
    (9, Required::Y, "договор подряда"),
    (9, Required::Y, "доп. соглашение"),
    (5, Required::Y, "номер документа"),
    (3, Required::Y, "наименование работ и затрат"),
    (11, Required::N, "зтр всего чел.-час"),
    (0, Required::N, "итого по акту:"),
    (3, Required::Y, "стоимость материальных ресурсов (всего)"),
];

#[derive(Debug, Clone)]
pub struct DesiredData {
    pub name: &'static str,
    pub offset: Option<(&'static str, (i8, i8))>,
}

#[rustfmt::skip]
pub const DESIRED_DATA_ARRAY: [DesiredData; 16] = [
    DesiredData{name:"Исполнитель",                  offset: None},
    DesiredData{name:"Глава",                        offset: None},
    DesiredData{name:"Глава наименование",           offset: None},
    DesiredData{name:"Объект",                       offset: Some(("объект",                         (0, 3)))},
    DesiredData{name:"Договор №",                    offset: Some(("договор подряда",                (0, 2)))},
    DesiredData{name:"Договор дата",                 offset: Some(("договор подряда",                (1, 2)))},
    DesiredData{name:"Смета №",                      offset: Some(("договор подряда",                (0, -9)))},
    DesiredData{name:"Смета наименование",           offset: Some(("договор подряда",                (1, -9)))},
    DesiredData{name:"По смете в ц.2000г.",          offset: Some(("доп. соглашение",                (0, -4)))},
    DesiredData{name:"Выполнение работ в ц.2000г.",  offset: Some(("доп. соглашение",                (1, -4)))},
    DesiredData{name:"Акт №",                        offset: Some(("номер документа",                (2, 0)))},
    DesiredData{name:"Акт дата",                     offset: Some(("номер документа",                (2, 4)))},
    DesiredData{name:"Отчетный период начало",       offset: Some(("номер документа",                (2, 5)))},
    DesiredData{name:"Отчетный период окончание",    offset: Some(("номер документа",                (2, 6)))},
    DesiredData{name:"Метод расчета",                offset: Some(("наименование работ и затрат",    (-1, -3)))},
    DesiredData{name:"Затраты труда, чел.-час",      offset: None},
];

pub struct ExtractedXlBooks {
    pub books: Vec<Result<Book, XlsxError>>,
    pub file_count_excluded: usize,
}

pub struct Book {
    pub path: PathBuf,
    pub data: Xlsx<BufReader<File>>,
}

impl Book {
    pub fn new(path: PathBuf) -> Result<Self, XlsxError> {
        let data: Xlsx<_> = calamine::open_workbook(&path)?;
        Ok(Book { path, data })
    }
}

pub struct Sheet {
    pub path: PathBuf,
    pub sheet_name: String,
    pub data: Range<DataType>,
    pub search_points: HashMap<&'static str, (usize, usize)>,
    pub range_start: (usize, usize),
}

impl<'a> Sheet {
    pub fn new(
        // разработчики Calamine делают зачем-то &mut self в функции worksheet_range(&mut self, name: &str),
        // из-за этого workbook приходится держать мутабельным, хотя этот код его менять вовсе не собирается
        // (из-за мутабельности workbook проблема при попытке множественных ссылок: можно только клонировать)
        workbook: &'a mut Book,
        user_entered_sh_name: &'a str,
        expected_sum_of_requir_col: usize,
    ) -> Result<Sheet, Error<'a>> {
        let entered_sh_name_lowercase = user_entered_sh_name.to_lowercase();

        let sheet_name = workbook
            .data
            .sheet_names()
            .iter()
            .find(|name| name.to_lowercase() == entered_sh_name_lowercase)
            .ok_or(Error::CalamineSheetOfTheBookIsUndetectable {
                file_path: &workbook.path,
                sh_name_for_search: user_entered_sh_name,
                sh_names: workbook.data.sheet_names().to_owned(),
            })?
            .clone();

        let xl_sheet = workbook
            .data
            .worksheet_range(&sheet_name)
            .ok_or(Error::CalamineSheetOfTheBookIsUndetectable {
                file_path: &workbook.path,
                sh_name_for_search: user_entered_sh_name,
                sh_names: workbook.data.sheet_names().to_owned(),
            })?
            .or_else(|error| {
                Err(Error::CalamineSheetOfTheBookIsUnreadable {
                    file_path: &workbook.path,
                    sh_name: sheet_name.to_owned(),
                    err: error,
                })
            })?;

        // это номера строки и столбца, с которых начинается диапазон данных листа
        // при ошибки передается точное имя листа (с учетом регистра, в отличии от имени, введеного пользователем)
        let sheet_start_coords = xl_sheet.start().ok_or(Error::EmptySheetRange {
            file_path: &workbook.path,
            sh_name: sheet_name.to_owned(),
        })?;

        let mut search_points = HashMap::new();

        let mut temp_sh_iter = xl_sheet.used_cells();
        let mut temp;
        for item in SEARCH_REFERENCE_POINTS {
            match item.1 {
                // Для Y-типов подходит расходуемый итератор - достигается проверка по очередности вохождения слов по строкам
                // (т.е. "Стройку" мы ожидаем выше "Объекта, например")
                Required::Y => {
                    temp = temp_sh_iter.find(|x| {
                        x.2.get_string()
                            .as_ref()
                            .unwrap_or_else(|| &"")
                            .to_lowercase()
                            == item.2
                    });
                }
                // Для N-типов нельзя использовать расходуемые итераторы, так как необязательное значение может и отсутсвовать (и при его поиске израсходуется итератор)
                Required::N => {
                    temp = xl_sheet.used_cells().find(|x| {
                        x.2.get_string()
                            .as_ref()
                            .unwrap_or_else(|| &"")
                            .to_lowercase()
                            == item.2
                    });
                }
            }

            if let Some((row, col, _)) = temp {
                search_points.insert(item.2, (row, col));
            }
        }

        // Проверка на полноту данных
        let test = SEARCH_REFERENCE_POINTS
            .iter()
            .filter(|x| x.1 == Required::Y)
            .last()
            .unwrap_or_else(|| panic!("ложь: \"DESIRED_DATA_ARRAY всегда имеет значения\""))
            .2;

        search_points
            .get(test)
            .ok_or(Error::SheetNotContainAllNecessaryData(&workbook.path))?;

        // Проверка значений на удаленность столбцов, чтобы гарантировать что найден нужный лист.
        let first_col = search_points
            .get("стройка")
            .unwrap_or_else(|| panic!("ложь: \"Необеспечены действительные имена HashMap\""));

        let (just_a_amount_requir_col, just_a_sum_requir_col) = SEARCH_REFERENCE_POINTS
            .iter()
            .fold((0_usize, 0), |acc, item| match item.1 {
                Required::Y => (
                    acc.0 + 1,
                    acc.1
                        + search_points
                            .get(item.2)
                            .unwrap_or_else(|| {
                                panic!("ложь: \"Необеспечены действительные имена HashMap\"")
                            })
                            .1,
                ),
                _ => acc,
            });

        if let false = just_a_sum_requir_col - first_col.1 * just_a_amount_requir_col
            == expected_sum_of_requir_col
        {
            return Err(Error::ShiftedColumnsInHeader(&workbook.path));
        }

        let range_start = (sheet_start_coords.0 as usize, sheet_start_coords.1 as usize);

        Ok(Sheet {
            path: workbook.path.clone(),
            sheet_name,
            data: xl_sheet,
            search_points,
            range_start,
        })
    }
}

pub fn extract_xl_books(path: &PathBuf) -> (Result<ExtractedXlBooks, Error<'static>>) {
    let files: Vec<_> = WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok()) //будет молча пропускать каталоги, на доступ к которым у владельца запущенного процесса нет разрешения
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|s| !s.starts_with('~') & s.ends_with(XL_FILE_EXTENSION))
                .unwrap_or_else(|| false)
        })
        .collect();

    let mut xl_files_vec = vec![];
    let mut file_count_excluded = 0;
    let mut file_print_counter = 0;

    for entry in files {
        let file_checked_path = entry
            .path()
            .strip_prefix(path)
            .map_err(|err| Error::InternalLogic {
                tech_descr: format!(
                    r#"Не удалось выполнить проверку на наличие символа "@" в пути для файла:
{}"#,
                    entry.path().to_string_lossy()
                ),
                err: Some(Box::new(err)),
            })?
            .to_string_lossy();

        if path.is_dir() {
            if file_checked_path.contains('@') {
                file_count_excluded += 1;
                continue;
            }
        }

        if xl_files_vec.len() == 0 {
            ui::display_formatted_text("\nОтбранны файлы:", None);
        }

        let file_display_path = if path.is_dir() {
            file_checked_path
        } else {
            path.to_string_lossy()
        };

        file_print_counter += 1;
        let msg = format!("{}: {}", file_print_counter, file_display_path);
        ui::display_formatted_text(&msg, None);

        let xl_file = Book::new(entry.into_path());
        xl_files_vec.push(xl_file);
    }

    Ok(ExtractedXlBooks {
        books: xl_files_vec,
        file_count_excluded,
    })
}
