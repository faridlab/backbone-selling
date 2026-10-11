// This directory's extension (hand-written; ADR-0031). The generated mod.rs (lib.rs for
// src/) pulls it in with include!, so its lines resolve against this directory.

// The hand-written selling SQL's parameter/projection types. Their repositories are declared
// `user_owned` in metaphor.codegen.yaml; these types mirror COLUMNS, not entities.
pub use quotation_repository::{
    AcceptedQuotationRow, NewQuotationRow, QuotationConversionRow, QuotationInvoiceHeaderRow,
    QuotationStatusRow,
};
pub use quotation_item_repository::{
    InvoicePolicyLineRow, NewQuotationItemRow, QuotationLineRow,
};
pub use quotation_template_repository::{NewQuotationTemplateRow, QuotationTemplateRow};
pub use sales_order_repository::{
    CancelRefusalRow, CancelledOrderRow, ConfirmedOrderRow, InvoiceSourceOrderRow,
    InvoiceStatusHeaderRow, NewSalesOrderRow, OrderFulfillmentHeaderRow, OrderStockHeaderRow,
    SalesOrderRefRow,
};
pub use sales_order_item_repository::{
    CostStampLineRow, InvoicePolicyOrderLineRow, LineBillingCapacityRow, LineDeliveryCapacityRow,
    LinePatchRow, MarginLineRow, MarginRollupRow, NewSalesOrderItemRow,
    OrderBillingRemainderRow, OrderDeliveryRemainderRow, OrderLineForInvoiceRow,
    OrderStockLineRow, WatermarkRollupRow,
};
// The carrier master's row type (its insert takes plain parameters — no New* row struct exists).
pub use delivery_carrier_repository::DeliveryCarrierRow;
pub use expense_reinvoice_link_repository::{
    ExpenseReinvoiceLinkRow, NewExpenseReinvoiceLinkRow,
};
