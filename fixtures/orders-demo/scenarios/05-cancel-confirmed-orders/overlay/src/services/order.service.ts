import { Order, OrderItem, OrderStatus, User } from "../domain/order";
import { ForbiddenError, InvalidStateError } from "../domain/errors";
import { OrderRepository } from "../repositories/order.repository";
import { OrderPolicy } from "../policies/order.policy";
import { InventoryService } from "./inventory.service";
import { PaymentService } from "./payment.service";
import { EventBus } from "../infra/events";
import { newId } from "../infra/ids";

export class OrderService {
  constructor(
    private readonly orders: OrderRepository,
    private readonly payments: PaymentService,
    private readonly inventory: InventoryService,
    private readonly policy: OrderPolicy,
    private readonly events: EventBus,
  ) {}

  async createOrder(customerId: string, items: OrderItem[]): Promise<Order> {
    await this.inventory.validateInventory(items);
    const order: Order = {
      id: newId(),
      customerId,
      status: OrderStatus.PENDING,
      paymentId: null,
      items,
    };
    await this.orders.insert(order);
    await this.inventory.reserve(items);
    this.events.emit("OrderCreated", { orderId: order.id });
    return order;
  }

  async confirmOrder(orderId: string): Promise<void> {
    const order = await this.orders.findById(orderId);
    if (order.status !== OrderStatus.PENDING) {
      throw new InvalidStateError("only pending orders can be confirmed");
    }
    order.status = OrderStatus.CONFIRMED;
    await this.orders.update(order);
  }

  async markPaid(orderId: string, paymentId: string): Promise<void> {
    const order = await this.orders.findById(orderId);
    if (order.status !== OrderStatus.CONFIRMED) {
      throw new InvalidStateError("only confirmed orders can be paid");
    }
    order.status = OrderStatus.PAID;
    order.paymentId = paymentId;
    await this.orders.update(order);
  }

  async cancelOrder(user: User, orderId: string): Promise<void> {
    const order = await this.orders.findById(orderId);
    if (!this.policy.canCancel(user, order)) {
      throw new ForbiddenError("not allowed to cancel");
    }
    if (order.status !== OrderStatus.PENDING && order.status !== OrderStatus.CONFIRMED) {
      throw new InvalidStateError("only pending or confirmed orders can be cancelled");
    }
    order.status = OrderStatus.CANCELLED;
    await this.orders.update(order);
    await this.inventory.release(order.items);
    if (order.paymentId) {
      await this.payments.refund(order.paymentId);
    }
    this.events.emit("OrderCancelled", { orderId: order.id });
  }
}
